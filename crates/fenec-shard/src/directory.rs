//! Where each tenant lives: the router's only state.
//!
//! Kept in a fenecdb file of its own -- two collections, `nodes` and
//! `tenants` -- and mirrored in maps for the request path, which never
//! touches the database. Every change goes to the file first and is synced
//! before the map changes, so a router that dies mid-operation comes back
//! with a directory that is at most one step behind, never ahead.
//!
//! A placement moves in one statement (`node` and `state` together), so
//! there is no moment on disk where a tenant belongs to both nodes or to
//! neither.
//!
//! A node is in a region and a zone (a datacenter), and a tenant is in its
//! node's region: kept on the node alone, so a move or a failover changes a
//! tenant's region with its node, and there is no second record to drift
//! from it.
//!
//! **A region may keep its own directory** (`fenec-shard --region`). Its
//! router holds the region's nodes and tenants in a file of its own, the
//! same collections, and leases and fails them over with no word from any
//! other region. The global directory then holds a home for each of its
//! tenants -- a row of `tenants` whose node is `@<region>`, a name no node
//! can have -- and the region's routers (`regions`). So the global
//! directory still holds every tenant's name, which a create anywhere is
//! checked against, and a router from before regional directories reading
//! it refuses whatever it is asked of such a tenant -- no node `@us` -- and
//! leases none of the region's nodes, which it does not know, rather than
//! route by a placement it cannot see. A regional directory names its
//! region in `scope`, so a router opened over the wrong file refuses it.

use fenec_core::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, RwLock};

/// What a tenant's node is in the global directory when the tenant lives
/// in a region keeping its own: `@` and the region.
pub const HOME: char = '@';

const SCHEMA: [&str; 10] = [
    "create collection if not exists nodes \
     (name text @hash, addr text, token text, region text, zone text)",
    "create collection if not exists tenants (name text @hash, node text @hash, state text)",
    // Which node replicates which. A collection of its own rather than a
    // field on `nodes`: a directory written before this existed opens as it
    // is, and the engine had no `alter` to add one then.
    "create collection if not exists pairs (node text @hash, standby text)",
    // Which node holds a tenant's replica, where it has one of its own
    // rather than its node's standby -- a collection of its own for the
    // reason `pairs` is.
    "create collection if not exists replicas (tenant text @hash, node text @hash)",
    // What a tenant's quota is set to over its node's (`fenec_http::quota`),
    // as the JSON a node is sent: kept here so that every node the tenant
    // is placed on -- its primary, its standby, its replica, the node it
    // moves or fails over to -- is sent it from one place.
    "create collection if not exists quotas (tenant text @hash, quota text)",
    // The regions keeping their own directory, and their routers' URLs,
    // comma-separated, the one keeping the directory first: the global
    // directory's alone.
    "create collection if not exists regions (name text @hash, routers text)",
    // The region a regional directory holds: one row, none in the global
    // directory.
    "create collection if not exists scope (region text)",
    // A tenant's read replicas: a follower of its primary on a node of
    // another region, which a router near that region answers the reads
    // its clients mark from. Kept in the directory holding the node -- a
    // region keeping its own directory keeps its own -- and keyed
    // `<tenant>/<node>`, one row a replica, since a tenant has one in each
    // of several regions. `follows` is the primary's address it was last
    // told to follow, which the router compares with where the primary is;
    // `lineage` the tenant's it copies (`lineages`), which a primary of
    // another -- the name made again -- it is taken off rather than
    // pointed at.
    "create collection if not exists readers \
     (key text @hash, tenant text @hash, node text @hash, follows text, lineage text)",
    // The regions keeping their own directory a read replica of a tenant's
    // name may be kept in, as their routers said placing one -- the global
    // directory's alone, keyed `<tenant>/<region>`. A create of the name
    // asks those regions alone whether one is still kept: every region,
    // one out of reach refused every create. A row outlives its replica
    // where the router taking it off could not say so, and goes once the
    // region says it keeps none. Read from the database where asked, never
    // mapped: a create reads it, nothing routes by it.
    "create collection if not exists traces (key text @hash, tenant text @hash, region text)",
    // Each tenant's lineage, as its node said when it made the tenant or
    // took its image in: the top half of its history's first id, 8 hex
    // digits, which a failover and a move keep and a tenant made again
    // under the name does not. Its read replicas are bound to it. `was`
    // the lineages it succeeded, space-separated: where the tenant's
    // primary came to be of another -- another file of it promoted by
    // force (a node refuses a copy of another lineage otherwise), an older
    // file restored, a file put in its place by hand -- the primary is the
    // tenant, and the replicas bound to one it was are pointed at it, not
    // taken off as another tenant's. `history` (`LINEAGE_FIELDS`) the
    // history its primary forked at its last failover, 16 hex digits,
    // which the copy promoted at the next must hold.
    "create collection if not exists lineages (tenant text @hash, lineage text, was text)",
];

/// The fields `nodes` took after directories were first written, added to
/// one written before them as the engine adds a field: its nodes read them
/// as null, the unnamed region and zone. A binary from before them reads
/// `name, addr, token` as it always did -- it forwards every tenant where
/// it is, and places without regard to regions.
const NODE_FIELDS: [&str; 2] = ["region", "zone"];

/// The fields `readers` took after it was first written: `lineage`, added
/// to one written before it as `NODE_FIELDS` are. Selected only where the
/// collection has it ([`reader_fields`]): asked of one without it, the
/// select failed with the field not found, which read as an empty
/// directory -- every read replica record forgotten.
const READER_FIELDS: [&str; 1] = ["lineage"];

/// The fields `lineages` took after it was first written: `history`,
/// added and selected as `READER_FIELDS` are. A router from before it
/// writes a tenant's lineage without it, which leaves the next promotion
/// held to the lineage alone.
const LINEAGE_FIELDS: [&str; 1] = ["history"];

/// A tenant's lineage, and the ones it succeeded, oldest first.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Lineage {
    pub now: String,
    pub was: Vec<String>,
    /// The history the tenant's primary forked at its last failover, as
    /// the promotion answered it ([`Directory::set_history`]).
    pub history: Option<String>,
}

impl Lineage {
    fn of(row: &[Value]) -> Lineage {
        Lineage {
            now: text(&row[1]),
            was: match row.get(2) {
                Some(Value::Text(w)) => w.split_whitespace().map(str::to_string).collect(),
                _ => Vec::new(),
            },
            history: match row.get(3) {
                Some(Value::Text(h)) => Some(h.clone()),
                _ => None,
            },
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Node {
    pub addr: String,
    /// The node's `--admin-token`. Never leaves the router.
    pub token: String,
    /// Where the node runs: a tenant lives in one region, and its replica
    /// is kept in the same one. Empty: the unnamed region, one region like
    /// any other, which every node of a directory that names none is in.
    pub region: String,
    /// The datacenter inside the region: a replica goes to another zone
    /// than its primary's where the region has one. Empty: not known.
    pub zone: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Active,
    /// A move started and has not been recorded as finished. The tenant is
    /// still served from `node`; the state only says a copy may exist on
    /// another node, which the next move of it clears.
    Moving,
}

impl State {
    fn as_str(self) -> &'static str {
        match self {
            State::Active => "active",
            State::Moving => "moving",
        }
    }

    fn parse(s: &str) -> State {
        match s {
            "moving" => State::Moving,
            _ => State::Active,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    pub node: String,
    pub state: State,
}

/// Which node a node's tenants are replicated to.
pub type Pairs = HashMap<String, String>;

/// Which node holds a tenant's replica.
pub type Replicas = HashMap<String, String>;

/// A tenant's read replica: the node holding it, in a region other than
/// the tenant's, and the primary's address it follows (`http://host:port`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reader {
    pub node: String,
    pub follows: String,
    /// The lineage of the tenant it copies, where it was known as it was
    /// placed (`lineages`).
    pub lineage: Option<String>,
}

pub struct Directory {
    /// Shared, because a standby follows it: the follower thread applies the
    /// primary's writes to the same database (`fenec-shard --replica-of`).
    db: Arc<RwLock<Database>>,
    /// The change counter the maps were read at, and the images adopted by
    /// then. A standby's database moves under them as the primary's writes
    /// arrive, and [`Self::refresh`] reads them again when it has. The
    /// counter alone missed an image landing on the change it stood at: a
    /// router rejoining as a standby kept routing from its old maps.
    seq: u64,
    adopted: u64,
    nodes: BTreeMap<String, Node>,
    tenants: HashMap<String, Placement>,
    /// The tenants living in a region that keeps its own directory, and
    /// the region: the `@<region>` rows of `tenants`, kept apart so that
    /// nothing reading placements meets a node no directory has.
    homes: HashMap<String, String>,
    pairs: Pairs,
    replicas: Replicas,
    /// Each tenant's quota, where one is set: the JSON a node is sent.
    quotas: HashMap<String, String>,
    /// Each tenant's read replicas on this directory's nodes, a region
    /// each, sorted by node.
    readers: HashMap<String, Vec<Reader>>,
    /// Each tenant's lineage, where its node said it, and the ones it
    /// succeeded (`lineages`): read by every look for every tenant with a
    /// read replica, and by every placement another region asks for -- a
    /// statement parsed and run each, read from the database.
    lineages: HashMap<String, Lineage>,
    /// The regions keeping their own directory, and their routers.
    regions: BTreeMap<String, Vec<String>>,
    /// The region this directory holds, where it is a region's own.
    scope: Option<String>,
    /// Whether the maps exist: a standby's arrive with the primary's first
    /// writes, and until then it knows of no tenant rather than that there
    /// is none.
    arrived: bool,
    /// The document behind each map entry, so a deletion the change ring
    /// reports by id finds the entry it takes out. Kept by a read of the
    /// maps; a write of this router's own leaves them behind, and the next
    /// refresh reads everything again.
    ids: Ids,
}

/// Document id -> key, one map a collection: nodes, tenants, pairs,
/// replicas, quotas, regions, scope, readers.
#[derive(Default)]
struct Ids {
    nodes: HashMap<DocId, String>,
    tenants: HashMap<DocId, String>,
    pairs: HashMap<DocId, String>,
    replicas: HashMap<DocId, String>,
    quotas: HashMap<DocId, String>,
    regions: HashMap<DocId, String>,
    scope: HashMap<DocId, String>,
    readers: HashMap<DocId, String>,
    lineages: HashMap<DocId, String>,
    kept: bool,
}

impl Directory {
    /// Opens (or creates) the directory file.
    pub fn open(path: impl AsRef<Path>) -> Result<Directory> {
        Directory::load(Arc::new(RwLock::new(fenec_core::fs::open(path)?)))
    }

    /// A directory that lives only as long as the process: tests.
    pub fn in_memory() -> Directory {
        Directory::load(Arc::new(RwLock::new(Database::new()))).expect("an empty database loads")
    }

    /// A directory over a database the caller already owns -- the one a
    /// standby's follower writes into, and a primary's feed reads from.
    pub fn load(db: Arc<RwLock<Database>>) -> Result<Directory> {
        {
            let mut g = write(&db);
            // A standby's collections arrive with the primary's writes; it
            // refuses writes of its own, this one included.
            if !g.history().following {
                for sql in SCHEMA {
                    g.execute(&fenec_ql::parse_one(sql)?)?;
                }
                for (coll, fields) in [
                    ("nodes", &NODE_FIELDS[..]),
                    ("readers", &READER_FIELDS[..]),
                    ("lineages", &LINEAGE_FIELDS[..]),
                ] {
                    for f in fields {
                        if g.collection(coll)?.schema.field_pos(f).is_none() {
                            let sql = format!("alter collection {coll} add field {f} text");
                            g.execute(&fenec_ql::parse_one(&sql)?)?;
                        }
                    }
                }
            }
        }
        let mut d = Directory {
            db,
            seq: 0,
            adopted: 0,
            nodes: BTreeMap::new(),
            tenants: HashMap::new(),
            homes: HashMap::new(),
            pairs: Pairs::new(),
            replicas: Replicas::new(),
            quotas: HashMap::new(),
            readers: HashMap::new(),
            lineages: HashMap::new(),
            regions: BTreeMap::new(),
            scope: None,
            arrived: false,
            ids: Ids::default(),
        };
        d.reload()?;
        Ok(d)
    }

    /// The database itself: what the replication endpoints stream from and
    /// the follower applies to.
    pub fn db(&self) -> &Arc<RwLock<Database>> {
        &self.db
    }

    /// Whether this directory follows another router's.
    pub fn following(&self) -> bool {
        read(&self.db).history().following
    }

    /// Whether the database has moved since the maps were read: on a standby
    /// every write the primary sent moves it.
    pub fn stale(&self) -> bool {
        let g = read(&self.db);
        g.change_seq() != self.seq || g.adoptions() != self.adopted
    }

    /// Brings the maps up to the database when it has moved: from the
    /// changes since they were read where the change ring can say what
    /// they were, and by reading them again where it cannot.
    pub fn refresh(&mut self) -> Result<()> {
        if self.stale() && !self.catch_up()? {
            self.reload()?;
        }
        Ok(())
    }

    fn reload(&mut self) -> Result<()> {
        let g = read(&self.db);
        let mut ids = Ids {
            kept: true,
            ..Ids::default()
        };
        let mut nodes = BTreeMap::new();
        let select = format!("get nodes select {}", node_fields(&g).join(", "));
        for (id, row) in rows(&g, "nodes", &select)? {
            ids.nodes.insert(id, text(&row[0]));
            nodes.insert(text(&row[0]), node_of(&row));
        }
        let (mut tenants, mut homes) = (HashMap::new(), HashMap::new());
        for (id, row) in rows(&g, "tenants", "get tenants select name, node, state")? {
            ids.tenants.insert(id, text(&row[0]));
            put_tenant(&mut tenants, &mut homes, text(&row[0]), &row);
        }
        let mut pairs = Pairs::new();
        for (id, row) in rows(&g, "pairs", "get pairs select node, standby")? {
            ids.pairs.insert(id, text(&row[0]));
            pairs.insert(text(&row[0]), text(&row[1]));
        }
        let mut replicas = Replicas::new();
        for (id, row) in rows(&g, "replicas", "get replicas select tenant, node")? {
            ids.replicas.insert(id, text(&row[0]));
            replicas.insert(text(&row[0]), text(&row[1]));
        }
        let mut quotas = HashMap::new();
        for (id, row) in rows(&g, "quotas", "get quotas select tenant, quota")? {
            ids.quotas.insert(id, text(&row[0]));
            quotas.insert(text(&row[0]), text(&row[1]));
        }
        let mut regions = BTreeMap::new();
        for (id, row) in rows(&g, "regions", "get regions select name, routers")? {
            ids.regions.insert(id, text(&row[0]));
            regions.insert(text(&row[0]), routers_of(&row[1]));
        }
        let mut scope = None;
        for (id, row) in rows(&g, "scope", "get scope select region")? {
            ids.scope.insert(id, text(&row[0]));
            scope = Some(text(&row[0]));
        }
        let mut readers = HashMap::new();
        let select = format!("get readers select {}", reader_fields(&g).join(", "));
        for (id, row) in rows(&g, "readers", &select)? {
            ids.readers.insert(id, text(&row[0]));
            put_reader(&mut readers, &row);
        }
        let mut lineages = HashMap::new();
        let select = format!("get lineages select {}", lineage_fields(&g).join(", "));
        for (id, row) in rows(&g, "lineages", &select)? {
            ids.lineages.insert(id, text(&row[0]));
            lineages.insert(text(&row[0]), Lineage::of(&row));
        }
        self.seq = g.change_seq();
        self.adopted = g.adoptions();
        self.nodes = nodes;
        self.tenants = tenants;
        self.homes = homes;
        self.pairs = pairs;
        self.replicas = replicas;
        self.quotas = quotas;
        self.regions = regions;
        self.scope = scope;
        self.readers = readers;
        self.lineages = lineages;
        self.ids = ids;
        self.arrived = g.collection("tenants").is_ok();
        Ok(())
    }

    /// The maps brought up to the database from the changes since they were
    /// read. On a standby every change the primary makes lands here, and
    /// reading every map again held the router's write lock 6.4 ms at
    /// 10 000 tenants and 93 ms at 100 000. `false` when the changes cannot
    /// say what happened -- an image adopted, the ring outrun, a schema
    /// changed, the ids left behind -- and the maps are to be read again.
    fn catch_up(&mut self) -> Result<bool> {
        let db = Arc::clone(&self.db);
        let g = read(&db);
        if !self.arrived || !self.ids.kept || g.adoptions() != self.adopted {
            return Ok(false);
        }
        // All five read before any map changes, so a collection the ring
        // cannot answer for leaves the maps as they were for the reload.
        let changes = |coll: &str, fields: &[&str]| -> Result<Option<ChangeBatch>> {
            let project: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
            match g.changes_since(coll, self.seq, None, Some(&project), &[]) {
                Ok(Changes::Batch(b)) if !b.schema_changed => Ok(Some(b)),
                Ok(_) | Err(Error::NotFound(_)) => Ok(None),
                Err(e) => Err(e),
            }
        };
        // Collections a directory from before them lacks -- a standby's,
        // following a primary from before them -- have no changes; one
        // that is there and cannot say has the maps read again.
        let optional = |coll: &str, fields: &[&str]| -> Result<Option<Option<ChangeBatch>>> {
            if g.collection(coll).is_err() {
                return Ok(Some(None));
            }
            Ok(changes(coll, fields)?.map(Some))
        };
        let (
            Some(nodes),
            Some(tenants),
            Some(pairs),
            Some(replicas),
            Some(quotas),
            Some(regions),
            Some(scope),
            Some(readers),
            Some(lineages),
        ) = (
            changes("nodes", &node_fields(&g))?,
            changes("tenants", &["name", "node", "state"])?,
            changes("pairs", &["node", "standby"])?,
            changes("replicas", &["tenant", "node"])?,
            changes("quotas", &["tenant", "quota"])?,
            optional("regions", &["name", "routers"])?,
            optional("scope", &["region"])?,
            optional("readers", &reader_fields(&g))?,
            optional("lineages", &lineage_fields(&g))?,
        )
        else {
            return Ok(false);
        };
        let ids = &mut self.ids;
        apply(&mut ids.nodes, nodes, |k, v| match v {
            Some(v) => drop(self.nodes.insert(k.into(), node_of(v))),
            None => drop(self.nodes.remove(k)),
        });
        apply(&mut ids.tenants, tenants, |k, v| match v {
            Some(v) => put_tenant(&mut self.tenants, &mut self.homes, k.into(), v),
            None => {
                self.tenants.remove(k);
                self.homes.remove(k);
            }
        });
        if let Some(regions) = regions {
            apply(&mut ids.regions, regions, |k, v| match v {
                Some(v) => drop(self.regions.insert(k.into(), routers_of(&v[1]))),
                None => drop(self.regions.remove(k)),
            });
        }
        if let Some(scope) = scope {
            apply(&mut ids.scope, scope, |k, v| match v {
                Some(_) => self.scope = Some(k.into()),
                None => self.scope = None,
            });
        }
        if let Some(readers) = readers {
            apply(&mut ids.readers, readers, |k, v| match v {
                Some(v) => put_reader(&mut self.readers, v),
                None => drop_reader(&mut self.readers, k),
            });
        }
        if let Some(lineages) = lineages {
            apply(&mut ids.lineages, lineages, |k, v| match v {
                Some(v) => drop(self.lineages.insert(k.into(), Lineage::of(v))),
                None => drop(self.lineages.remove(k)),
            });
        }
        apply(&mut ids.pairs, pairs, |k, v| match v {
            Some(v) => drop(self.pairs.insert(k.into(), text(&v[1]))),
            None => drop(self.pairs.remove(k)),
        });
        apply(&mut ids.replicas, replicas, |k, v| match v {
            Some(v) => drop(self.replicas.insert(k.into(), text(&v[1]))),
            None => drop(self.replicas.remove(k)),
        });
        apply(&mut ids.quotas, quotas, |k, v| match v {
            Some(v) => drop(self.quotas.insert(k.into(), text(&v[1]))),
            None => drop(self.quotas.remove(k)),
        });
        self.seq = g.change_seq();
        Ok(true)
    }

    /// Whether the maps have arrived; see the field.
    pub fn arrived(&self) -> bool {
        self.arrived
    }

    /// The node `name`'s tenants are replicated to, if any.
    pub fn standby(&self, name: &str) -> Option<&str> {
        self.pairs.get(name).map(String::as_str)
    }

    /// Whether `name` is some node's standby: its tenants follow that
    /// node's, so a tenant is never placed or moved there -- it would open
    /// as a replica of one the other node does not have.
    pub fn is_standby(&self, name: &str) -> bool {
        self.pairs.values().any(|s| s == name)
    }

    /// Records (or clears) the node a node's tenants are replicated to.
    pub fn set_standby(&mut self, node: &str, standby: Option<&str>) -> Result<()> {
        match standby {
            None => {
                self.run("del pairs where node = $1", &[Value::Text(node.into())])?;
                self.pairs.remove(node);
            }
            Some(s) => {
                let params = [Value::Text(node.into()), Value::Text(s.into())];
                if self.pairs.contains_key(node) {
                    self.run("set pairs {standby: $2} where node = $1", &params)?;
                } else {
                    self.run("put pairs {node: $1, standby: $2}", &params)?;
                }
                self.pairs.insert(node.into(), s.into());
            }
        }
        Ok(())
    }

    /// The node holding a tenant's replica of its own, if it has one.
    pub fn replica(&self, tenant: &str) -> Option<&str> {
        self.replicas.get(tenant).map(String::as_str)
    }

    /// The tenants whose replica `node` holds, sorted.
    pub fn replicas_on(&self, node: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .replicas
            .iter()
            .filter(|(_, n)| n.as_str() == node)
            .map(|(t, _)| t.clone())
            .collect();
        out.sort();
        out
    }

    /// The replicas of `primary`'s tenants on each node that holds any.
    pub fn replicas_from(&self, primary: &str) -> BTreeMap<&str, usize> {
        let mut by = BTreeMap::new();
        for (t, n) in &self.replicas {
            if self.tenants.get(t).is_some_and(|p| p.node == primary) {
                *by.entry(n.as_str()).or_default() += 1;
            }
        }
        by
    }

    /// The replicas on each node, every node named.
    pub fn replicas_by_node(&self) -> BTreeMap<&str, usize> {
        let mut by: BTreeMap<&str, usize> = self.nodes.keys().map(|n| (n.as_str(), 0)).collect();
        for n in self.replicas.values() {
            *by.entry(n.as_str()).or_default() += 1;
        }
        by
    }

    /// Records (or clears) the node holding a tenant's replica.
    pub fn set_replica(&mut self, tenant: &str, node: Option<&str>) -> Result<()> {
        match node {
            None if self.replicas.contains_key(tenant) => {
                self.run(
                    "del replicas where tenant = $1",
                    &[Value::Text(tenant.into())],
                )?;
                self.replicas.remove(tenant);
            }
            None => {}
            Some(n) => {
                let params = [Value::Text(tenant.into()), Value::Text(n.into())];
                if self.replicas.contains_key(tenant) {
                    self.run("set replicas {node: $2} where tenant = $1", &params)?;
                } else {
                    self.run("put replicas {tenant: $1, node: $2}", &params)?;
                }
                self.replicas.insert(tenant.into(), n.into());
            }
        }
        Ok(())
    }

    /// The quota set for a tenant over its node's, as the JSON a node is
    /// sent (`PUT /_admin/tenants/<t>/quota`), where one is set.
    pub fn quota(&self, tenant: &str) -> Option<&str> {
        self.quotas.get(tenant).map(String::as_str)
    }

    /// Records (or clears) the quota set for a tenant.
    pub fn set_quota(&mut self, tenant: &str, quota: Option<&str>) -> Result<()> {
        match quota {
            None if self.quotas.contains_key(tenant) => {
                self.run(
                    "del quotas where tenant = $1",
                    &[Value::Text(tenant.into())],
                )?;
                self.quotas.remove(tenant);
            }
            None => {}
            Some(q) => {
                let params = [Value::Text(tenant.into()), Value::Text(q.into())];
                if self.quotas.contains_key(tenant) {
                    self.run("set quotas {quota: $2} where tenant = $1", &params)?;
                } else {
                    self.run("put quotas {tenant: $1, quota: $2}", &params)?;
                }
                self.quotas.insert(tenant.into(), q.into());
            }
        }
        Ok(())
    }

    pub fn nodes(&self) -> &BTreeMap<String, Node> {
        &self.nodes
    }

    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.get(name)
    }

    /// The region of the node `name`; the unnamed one for a node it does
    /// not know.
    pub fn region_of(&self, name: &str) -> &str {
        self.nodes.get(name).map_or("", |n| n.region.as_str())
    }

    /// The region a tenant lives in: its node's, or the region keeping its
    /// own directory it is at home in.
    pub fn tenant_region(&self, tenant: &str) -> Option<&str> {
        match self.tenants.get(tenant) {
            Some(p) => Some(self.region_of(&p.node)),
            None => self.home(tenant),
        }
    }

    /// The region a tenant lives in where that region keeps its own
    /// directory: the global directory's home for it.
    pub fn home(&self, tenant: &str) -> Option<&str> {
        self.homes.get(tenant).map(String::as_str)
    }

    /// Whether the directory holds `tenant` at all: placed, or at home in a
    /// region keeping its own directory.
    pub fn exists(&self, tenant: &str) -> bool {
        self.tenants.contains_key(tenant) || self.homes.contains_key(tenant)
    }

    /// `(tenant, region)` for each tenant at home in a region keeping its
    /// own directory, sorted by name.
    pub fn homes(&self) -> Vec<(String, String)> {
        let mut out: Vec<_> = self
            .homes
            .iter()
            .map(|(t, r)| (t.clone(), r.clone()))
            .collect();
        out.sort();
        out
    }

    /// The regions keeping their own directory, and their routers' URLs.
    pub fn regions(&self) -> &BTreeMap<String, Vec<String>> {
        &self.regions
    }

    /// The routers of `region`, where it keeps its own directory: the one
    /// keeping it first.
    pub fn routers(&self, region: &str) -> Option<&[String]> {
        self.regions.get(region).map(Vec::as_slice)
    }

    /// Records (or clears) a region keeping its own directory.
    pub fn set_region(&mut self, region: &str, routers: Option<&[String]>) -> Result<()> {
        match routers {
            None => {
                self.run("del regions where name = $1", &[Value::Text(region.into())])?;
                self.regions.remove(region);
            }
            Some(urls) => {
                let params = [Value::Text(region.into()), Value::Text(urls.join(","))];
                if self.regions.contains_key(region) {
                    self.run("set regions {routers: $2} where name = $1", &params)?;
                } else {
                    self.run("put regions {name: $1, routers: $2}", &params)?;
                }
                self.regions.insert(region.into(), urls.to_vec());
            }
        }
        Ok(())
    }

    /// The region this directory holds, where it is a region's own.
    pub fn scope(&self) -> Option<&str> {
        self.scope.as_deref()
    }

    /// Checks that this directory is the one a router keeping `region`'s --
    /// or, `None`, the global one -- opens, and marks a new one as
    /// `region`'s. A directory that follows another is not checked here:
    /// its rows arrive later, the scope among them.
    pub fn settle_scope(&mut self, region: Option<&str>) -> std::result::Result<(), String> {
        if self.following() {
            return Ok(());
        }
        match (region, self.scope.clone()) {
            (None, None) => Ok(()),
            (Some(r), Some(s)) if r == s => Ok(()),
            (None, Some(s)) => Err(format!(
                "this directory is region `{s}`'s own: start the router with --region {s}"
            )),
            (Some(r), Some(s)) => Err(format!("this directory is region `{s}`'s own, not `{r}`'s")),
            (Some(r), None)
                if self.nodes.is_empty() && self.tenants.is_empty() && self.homes.is_empty() =>
            {
                self.run("put scope {region: $1}", &[Value::Text(r.into())])
                    .map_err(|e| e.to_string())?;
                self.scope = Some(r.into());
                Ok(())
            }
            (Some(r), None) => Err(format!(
                "this directory holds nodes or tenants and names no region: it is a global \
                 directory, which --region {r} does not open; give the region a file of its own"
            )),
        }
    }

    /// Whether the nodes `a` and `b` are known to be in one zone: both
    /// zones named, and the same. A zone not named is not known to be any.
    pub fn same_zone(&self, a: &str, b: &str) -> bool {
        match (self.nodes.get(a), self.nodes.get(b)) {
            (Some(a), Some(b)) => !a.zone.is_empty() && a.zone == b.zone,
            _ => false,
        }
    }

    /// Whether the node `name` holds what a region is promised to: a
    /// tenant, a tenant's replica, or a place in a pair. Its region is fixed
    /// while it does -- a tenant's residency would change under it, and a
    /// copy would sit in another region than its primary.
    pub fn holds(&self, name: &str) -> bool {
        self.tenants.values().any(|p| p.node == name)
            || self.replicas.values().any(|n| n == name)
            || self.pairs.contains_key(name)
            || self.is_standby(name)
            || self.readers.values().flatten().any(|r| r.node == name)
    }

    /// A tenant's read replicas on this directory's nodes.
    pub fn readers(&self, tenant: &str) -> &[Reader] {
        self.readers.get(tenant).map_or(&[], Vec::as_slice)
    }

    /// A tenant's read replica on a node of `region`, if it has one here.
    pub fn reader_in(&self, tenant: &str, region: &str) -> Option<&Reader> {
        self.readers(tenant)
            .iter()
            .find(|r| self.region_of(&r.node) == region)
    }

    /// `(tenant, reader)` for every read replica this directory keeps,
    /// sorted: what the router keeps following its tenants' primaries.
    pub fn all_readers(&self) -> Vec<(String, Reader)> {
        let mut out: Vec<(String, Reader)> = self
            .readers
            .iter()
            .flat_map(|(t, rs)| rs.iter().map(move |r| (t.clone(), r.clone())))
            .collect();
        out.sort_by(|a, b| (&a.0, &a.1.node).cmp(&(&b.0, &b.1.node)));
        out
    }

    /// The read replicas on each node, every node named.
    pub fn readers_by_node(&self) -> BTreeMap<&str, usize> {
        let mut by: BTreeMap<&str, usize> = self.nodes.keys().map(|n| (n.as_str(), 0)).collect();
        for r in self.readers.values().flatten() {
            *by.entry(r.node.as_str()).or_default() += 1;
        }
        by
    }

    /// Records a tenant's read replica on `node`, following `follows`, a
    /// copy of the tenant of `lineage`.
    pub fn set_reader(
        &mut self,
        tenant: &str,
        node: &str,
        follows: &str,
        lineage: Option<&str>,
    ) -> Result<()> {
        let params = [
            Value::Text(format!("{tenant}/{node}")),
            Value::Text(tenant.into()),
            Value::Text(node.into()),
            Value::Text(follows.into()),
            lineage.map_or(Value::Null, |l| Value::Text(l.into())),
        ];
        if self.readers(tenant).iter().any(|r| r.node == node) {
            self.run(
                "set readers {follows: $4, lineage: $5} where key = $1",
                &params,
            )?;
        } else {
            self.run(
                "put readers {key: $1, tenant: $2, node: $3, follows: $4, lineage: $5}",
                &params,
            )?;
        }
        put_reader(&mut self.readers, &params);
        Ok(())
    }

    /// `tenant`'s lineage, as its node said (`lineages`).
    pub fn lineage(&self, tenant: &str) -> Option<String> {
        self.lineages.get(tenant).map(|l| l.now.clone())
    }

    /// The lineages `tenant`'s succeeded ([`Directory::succeed_lineage`]),
    /// oldest first.
    pub fn former_lineages(&self, tenant: &str) -> &[String] {
        self.lineages.get(tenant).map_or(&[], |l| &l.was[..])
    }

    /// The history `tenant`'s primary forked at its last failover, as
    /// the promotion answered it: the copy promoted at the next must hold
    /// it among its own.
    pub fn history(&self, tenant: &str) -> Option<String> {
        self.lineages.get(tenant).and_then(|l| l.history.clone())
    }

    /// Records `tenant`'s lineage, or that it has none known: a tenant
    /// made, or its image taken in, which no lineage before it is.
    pub fn set_lineage(&mut self, tenant: &str, lineage: Option<&str>) -> Result<()> {
        let fresh = lineage.map(|l| Lineage {
            now: l.to_string(),
            ..Lineage::default()
        });
        self.put_lineage(tenant, fresh)
    }

    /// Records the history `tenant`'s primary forked as it was promoted,
    /// where its lineage is known: what the copy promoted then held of the
    /// tenant says nothing of the copy promoted at the next failover.
    pub fn set_history(&mut self, tenant: &str, history: &str) -> Result<()> {
        match self.history_change(tenant, history) {
            None => Ok(()),
            Some(stmt) => self.write_block(&[stmt]),
        }
        .map(|_| self.history_noted(tenant, history))
    }

    /// Records `tenant` placed on `node` as a promotion there left it, and
    /// the history that promotion forked ([`Self::set_history`]): one
    /// block, one sync -- the history a statement of its own was a second
    /// fsync for every tenant a failover promotes, under the router's
    /// write lock.
    pub fn place_promoted(
        &mut self,
        tenant: &str,
        node: &str,
        history: Option<&str>,
    ) -> Result<()> {
        let place = match self.exists(tenant) {
            true => "set tenants {node: $2, state: $3} where name = $1",
            false => "put tenants {name: $1, node: $2, state: $3}",
        };
        let params = vec![
            Value::Text(tenant.into()),
            Value::Text(node.into()),
            Value::Text(State::Active.as_str().into()),
        ];
        let mut stmts = vec![(place, params.clone())];
        stmts.extend(history.and_then(|h| self.history_change(tenant, h)));
        self.write_block(&stmts)?;
        put_tenant(&mut self.tenants, &mut self.homes, tenant.into(), &params);
        if let Some(h) = history {
            self.history_noted(tenant, h);
        }
        Ok(())
    }

    /// The statement recording `history` as `tenant`'s, where its lineage
    /// is known, the history is another and the collection has the field
    /// -- a standby router promoted where it stood, over a directory a
    /// router from before it wrote, never added it.
    fn history_change(&self, tenant: &str, history: &str) -> Option<(&'static str, Vec<Value>)> {
        let l = self.lineages.get(tenant)?;
        if l.history.as_deref() == Some(history) || lineage_fields(&read(&self.db)).len() <= 3 {
            return None;
        }
        Some((
            "set lineages {history: $2} where tenant = $1",
            vec![Value::Text(tenant.into()), Value::Text(history.into())],
        ))
    }

    /// `history` as `tenant`'s in the maps, where its lineage is known.
    fn history_noted(&mut self, tenant: &str, history: &str) {
        if let Some(l) = self.lineages.get_mut(tenant) {
            l.history = Some(history.to_string());
        }
    }

    /// Records that `tenant`'s primary is of `lineage` now, the one it was
    /// of kept among those it was: the same tenant, its primary promoted
    /// from an older file of it, or an older file of it put in its place.
    pub fn succeed_lineage(&mut self, tenant: &str, lineage: &str) -> Result<()> {
        let mut l = self.lineages.get(tenant).cloned().unwrap_or_default();
        if l.now == lineage {
            return Ok(());
        }
        if !l.now.is_empty() && !l.was.contains(&l.now) {
            l.was.push(std::mem::take(&mut l.now));
        }
        l.was.retain(|w| w != lineage);
        l.now = lineage.to_string();
        // Another file's: the history the one before forked is not in it.
        l.history = None;
        self.put_lineage(tenant, Some(l))
    }

    fn put_lineage(&mut self, tenant: &str, lineage: Option<Lineage>) -> Result<()> {
        self.run(
            "del lineages where tenant = $1",
            &[Value::Text(tenant.into())],
        )?;
        self.lineages.remove(tenant);
        let Some(l) = lineage else {
            return Ok(());
        };
        let was = match l.was.is_empty() {
            true => Value::Null,
            false => Value::Text(l.was.join(" ")),
        };
        let mut params = vec![Value::Text(tenant.into()), Value::Text(l.now.clone()), was];
        // Written where the collection has the field: a standby router
        // promoted where it stood, over a directory a router from before
        // it wrote, never added it.
        let kept = lineage_fields(&read(&self.db)).len() > 3;
        let sql = match &l.history {
            Some(h) if kept => {
                params.push(Value::Text(h.clone()));
                "put lineages {tenant: $1, lineage: $2, was: $3, history: $4}"
            }
            _ => "put lineages {tenant: $1, lineage: $2, was: $3}",
        };
        self.run(sql, &params)?;
        self.lineages.insert(tenant.into(), l);
        Ok(())
    }

    /// The regions a read replica of `tenant`'s name may be kept in
    /// (`traces`), sorted.
    pub fn traced_in(&self, tenant: &str) -> Result<Vec<String>> {
        let g = read(&self.db);
        let stmt = fenec_ql::parse_one("get traces select region where tenant = $1")?;
        let mut out = match g.query(&stmt, &[Value::Text(tenant.into())]) {
            Ok(Response::Rows(rs)) => rs.rows.into_iter().map(|r| text(&r.values[0])).collect(),
            Ok(_) | Err(Error::NotFound(_)) => Vec::new(),
            Err(e) => return Err(e),
        };
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// Notes that `region` may keep a read replica of `tenant`'s name.
    pub fn set_trace(&mut self, tenant: &str, region: &str) -> Result<()> {
        if self.traced_in(tenant)?.iter().any(|r| r == region) {
            return Ok(());
        }
        let params = [
            Value::Text(format!("{tenant}/{region}")),
            Value::Text(tenant.into()),
            Value::Text(region.into()),
        ];
        self.run("put traces {key: $1, tenant: $2, region: $3}", &params)
    }

    /// Notes that `region` keeps no read replica of `tenant`'s name.
    pub fn remove_trace(&mut self, tenant: &str, region: &str) -> Result<()> {
        let key = Value::Text(format!("{tenant}/{region}"));
        self.run("del traces where key = $1", &[key])
    }

    /// Forgets a tenant's read replica on `node`.
    pub fn remove_reader(&mut self, tenant: &str, node: &str) -> Result<()> {
        let key = format!("{tenant}/{node}");
        self.run("del readers where key = $1", &[Value::Text(key.clone())])?;
        drop_reader(&mut self.readers, &key);
        Ok(())
    }

    /// The node holding the copy a failover of `tenant` promotes -- its
    /// replica of its own, or its node's standby -- if it has one.
    pub fn copy_of(&self, tenant: &str) -> Option<&str> {
        self.replica(tenant).or_else(|| {
            let p = self.tenants.get(tenant)?;
            self.standby(&p.node)
        })
    }

    /// Per region, the tenants whose copy is known to be in their primary's
    /// zone -- a datacenter lost takes both -- every region a node names:
    /// what a scrape reports, counted in one pass.
    pub fn same_zone_copies(&self) -> BTreeMap<&str, usize> {
        let mut by: BTreeMap<&str, usize> = self
            .nodes
            .values()
            .map(|n| (n.region.as_str(), 0))
            .collect();
        for (t, p) in &self.tenants {
            if self.copy_of(t).is_some_and(|c| self.same_zone(&p.node, c)) {
                *by.entry(self.region_of(&p.node)).or_default() += 1;
            }
        }
        by
    }

    pub fn placement(&self, tenant: &str) -> Option<&Placement> {
        self.tenants.get(tenant)
    }

    /// The tenants on each node, every node named, and how many are in a
    /// move: what a scrape reports, counted without the copy
    /// [`Directory::tenants`] makes of 100 000 names.
    pub fn load_by_node(&self) -> (BTreeMap<&str, usize>, usize) {
        let mut by: BTreeMap<&str, usize> = self.nodes.keys().map(|n| (n.as_str(), 0)).collect();
        let mut moving = 0;
        for p in self.tenants.values() {
            *by.entry(p.node.as_str()).or_default() += 1;
            moving += (p.state == State::Moving) as usize;
        }
        (by, moving)
    }

    /// The tenants each node holds the primary of, sorted, every node named:
    /// what a node's lease names under automatic failover. One pass, not a
    /// [`Directory::tenants`] copy a node.
    pub fn primaries(&self) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> =
            self.nodes.keys().map(|n| (n.clone(), Vec::new())).collect();
        for (t, p) in &self.tenants {
            if let Some(list) = out.get_mut(&p.node) {
                list.push(t.clone());
            }
        }
        for list in out.values_mut() {
            list.sort_unstable();
        }
        out
    }

    /// `(tenant, placement)`, sorted by name.
    pub fn tenants(&self) -> Vec<(String, Placement)> {
        let mut out: Vec<_> = self
            .tenants
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Adds a node or changes its address, token, region and zone.
    pub fn set_node(&mut self, name: &str, node: Node) -> Result<()> {
        // The unnamed region and zone are null, as a node written before
        // the fields reads them.
        let named = |s: &str| match s {
            "" => Value::Null,
            s => Value::Text(s.into()),
        };
        let params = [
            Value::Text(name.into()),
            Value::Text(node.addr.clone()),
            Value::Text(node.token.clone()),
            named(&node.region),
            named(&node.zone),
        ];
        if self.nodes.contains_key(name) {
            self.run(
                "set nodes {addr: $2, token: $3, region: $4, zone: $5} where name = $1",
                &params,
            )?;
        } else {
            self.run(
                "put nodes {name: $1, addr: $2, token: $3, region: $4, zone: $5}",
                &params,
            )?;
        }
        self.nodes.insert(name.into(), node);
        Ok(())
    }

    /// Refused while a tenant is placed on it: the directory would point
    /// at a node it no longer knows how to reach.
    pub fn remove_node(&mut self, name: &str) -> Result<()> {
        if let Some((t, _)) = self.tenants.iter().find(|(_, p)| p.node == name) {
            return Err(Error::Query(format!(
                "node `{name}` still holds tenant `{t}`: move or delete its tenants first"
            )));
        }
        // A read replica's record let go of with its node left its copy
        // following its primary with nothing to stop it, should the node
        // come back: each is taken off first, or forgotten for a node gone.
        if let Some((t, _)) = self
            .readers
            .iter()
            .find(|(_, rs)| rs.iter().any(|r| r.node == name))
        {
            return Err(Error::Query(format!(
                "node `{name}` still holds a read replica of `{t}`: DELETE \
                 /_shard/tenants/{t}/read-replicas/<region> first, with ?forget for a node \
                 gone for good"
            )));
        }
        self.run("del nodes where name = $1", &[Value::Text(name.into())])?;
        self.nodes.remove(name);
        // The pairs it was on either side of go with it.
        self.set_standby(name, None)?;
        let holders: Vec<String> = self
            .pairs
            .iter()
            .filter(|(_, s)| s.as_str() == name)
            .map(|(n, _)| n.clone())
            .collect();
        for n in holders {
            self.set_standby(&n, None)?;
        }
        // And the replicas on it: those tenants have none until a repair.
        for t in self.replicas_on(name) {
            self.set_replica(&t, None)?;
        }
        Ok(())
    }

    /// Records where a tenant lives, and in what state: on a node, or with
    /// `node` [`HOME`] and a region, at home in a region keeping its own
    /// directory. One statement either way, so a tenant leaving for such a
    /// region, or coming back, is never in neither.
    pub fn place(&mut self, tenant: &str, node: &str, state: State) -> Result<()> {
        let params = [
            Value::Text(tenant.into()),
            Value::Text(node.into()),
            Value::Text(state.as_str().into()),
        ];
        if self.exists(tenant) {
            self.run("set tenants {node: $2, state: $3} where name = $1", &params)?;
        } else {
            self.run("put tenants {name: $1, node: $2, state: $3}", &params)?;
        }
        put_tenant(&mut self.tenants, &mut self.homes, tenant.into(), &params);
        Ok(())
    }

    /// Records `tenant` at home in `region`, which keeps its own directory.
    pub fn place_home(&mut self, tenant: &str, region: &str) -> Result<()> {
        self.place(tenant, &format!("{HOME}{region}"), State::Active)
    }

    /// Forgets a tenant: its placement or home, its quota and its replica.
    /// Not its read replicas, whose files the router takes off their nodes
    /// with them: a tenant leaving this directory for a region keeping its
    /// own keeps the ones here, which follow it there.
    pub fn remove_tenant(&mut self, tenant: &str) -> Result<()> {
        self.run("del tenants where name = $1", &[Value::Text(tenant.into())])?;
        self.tenants.remove(tenant);
        self.homes.remove(tenant);
        self.set_lineage(tenant, None)?;
        self.set_quota(tenant, None)?;
        self.set_replica(tenant, None)
    }

    /// One statement, then a sync: a directory change the router has
    /// acknowledged is on disk.
    fn run(&mut self, sql: &str, params: &[Value]) -> Result<()> {
        let stmt = fenec_ql::parse_one(sql)?;
        let mut g = write(&self.db);
        g.execute_with(&stmt, params)?;
        g.sync()?;
        // The maps are updated by the caller, not their ids; the counter
        // moved here.
        self.seq = g.change_seq();
        self.adopted = g.adoptions();
        self.ids.kept = false;
        Ok(())
    }

    /// Statements as one block, then one sync, as [`Self::run`] runs one.
    fn write_block(&mut self, stmts: &[(&str, Vec<Value>)]) -> Result<()> {
        let parsed = stmts
            .iter()
            .map(|(sql, _)| fenec_ql::parse_one(sql))
            .collect::<Result<Vec<_>>>()?;
        let block: Vec<(&Statement, &[Value])> = parsed
            .iter()
            .zip(stmts)
            .map(|(s, (_, params))| (s, &params[..]))
            .collect();
        let mut g = write(&self.db);
        g.execute_block(&block).map_err(|(_, e)| e)?;
        g.sync()?;
        // The maps are updated by the caller, not their ids; the counter
        // moved here.
        self.seq = g.change_seq();
        self.adopted = g.adoptions();
        self.ids.kept = false;
        Ok(())
    }
}

fn read(db: &Arc<RwLock<Database>>) -> std::sync::RwLockReadGuard<'_, Database> {
    db.read().unwrap_or_else(|e| e.into_inner())
}

fn write(db: &Arc<RwLock<Database>>) -> std::sync::RwLockWriteGuard<'_, Database> {
    db.write().unwrap_or_else(|e| e.into_inner())
}

/// One collection's changes into its map, through `edit`: a key and the
/// values to put under it, or none to take it out. A deletion takes out
/// the entry its id was behind, and a put the one its document had under
/// another key before it goes in under its own.
fn apply(
    ids: &mut HashMap<DocId, String>,
    batch: ChangeBatch,
    mut edit: impl FnMut(&str, Option<&[Value]>),
) {
    for id in batch.dels {
        if let Some(k) = ids.remove(&id) {
            edit(&k, None);
        }
    }
    for row in batch.puts.rows {
        let key = text(&row.values[0]);
        if let Some(old) = ids.insert(row.id, key.clone()) {
            if old != key {
                edit(&old, None);
            }
        }
        edit(&key, Some(&row.values));
    }
}

/// The fields a node is read by: its region and zone where the collection
/// has them -- a standby's, following a primary from before them, has not.
fn node_fields(db: &Database) -> Vec<&'static str> {
    let mut fields = vec!["name", "addr", "token"];
    if let Ok(c) = db.collection("nodes") {
        fields.extend(
            NODE_FIELDS
                .iter()
                .filter(|f| c.schema.field_pos(f).is_some()),
        );
    }
    fields
}

fn node_of(row: &[Value]) -> Node {
    Node {
        addr: text(&row[1]),
        token: text(&row[2]),
        region: row.get(3).map(text).unwrap_or_default(),
        zone: row.get(4).map(text).unwrap_or_default(),
    }
}

fn placement_of(row: &[Value]) -> Placement {
    Placement {
        node: text(&row[1]),
        state: State::parse(&text(&row[2])),
    }
}

/// A row of `tenants` (`name, node, state`) into the map it belongs in: a
/// placement, or a home in a region keeping its own directory -- and out
/// of the other, where it was there before.
fn put_tenant(
    tenants: &mut HashMap<String, Placement>,
    homes: &mut HashMap<String, String>,
    name: String,
    row: &[Value],
) {
    let p = placement_of(row);
    match p.node.strip_prefix(HOME) {
        Some(region) => {
            tenants.remove(&name);
            homes.insert(name, region.to_string());
        }
        None => {
            homes.remove(&name);
            tenants.insert(name, p);
        }
    }
}

/// A row of `readers` (`key, tenant, node, follows`) into the map, in the
/// place of the one of its node.
fn put_reader(readers: &mut HashMap<String, Vec<Reader>>, row: &[Value]) {
    let (tenant, node) = (text(&row[1]), text(&row[2]));
    let list = readers.entry(tenant).or_default();
    list.retain(|r| r.node != node);
    list.push(Reader {
        node,
        follows: text(&row[3]),
        lineage: match row.get(4) {
            Some(Value::Text(l)) => Some(l.clone()),
            _ => None,
        },
    });
    list.sort_by(|a, b| a.node.cmp(&b.node));
}

/// The read replica of `key` (`<tenant>/<node>`) out of the map.
fn drop_reader(readers: &mut HashMap<String, Vec<Reader>>, key: &str) {
    let Some((tenant, node)) = key.split_once('/') else {
        return;
    };
    if let Some(list) = readers.get_mut(tenant) {
        list.retain(|r| r.node != node);
        if list.is_empty() {
            readers.remove(tenant);
        }
    }
}

/// A region's routers as `regions` keeps them, comma-separated.
fn routers_of(v: &Value) -> Vec<String> {
    text(v)
        .split(',')
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .map(str::to_string)
        .collect()
}

fn rows(db: &Database, coll: &str, sql: &str) -> Result<Vec<(DocId, Vec<Value>)>> {
    match db.query(&fenec_ql::parse_one(sql)?, &[]) {
        Ok(Response::Rows(rs)) => Ok(rs.rows.into_iter().map(|r| (r.id, r.values)).collect()),
        Ok(_) => Ok(Vec::new()),
        // A standby's collections arrive with the primary's first writes:
        // until they do the directory is empty, not broken. A collection
        // that is there and answers not found -- a field it lacks -- is
        // the error it is: read as empty, it forgot every row.
        Err(Error::NotFound(_)) if db.collection(coll).is_err() => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// The fields of `readers` a select names: those the collection has, a
/// standby's following a primary from before `lineage` among them.
fn lineage_fields(db: &Database) -> Vec<&'static str> {
    let mut fields = vec!["tenant", "lineage", "was"];
    if let Ok(c) = db.collection("lineages") {
        fields.extend(
            LINEAGE_FIELDS
                .iter()
                .filter(|f| c.schema.field_pos(f).is_some()),
        );
    }
    fields
}

fn reader_fields(db: &Database) -> Vec<&'static str> {
    let mut fields = vec!["key", "tenant", "node", "follows"];
    if let Ok(c) = db.collection("readers") {
        fields.extend(
            READER_FIELDS
                .iter()
                .filter(|f| c.schema.field_pos(f).is_some()),
        );
    }
    fields
}

fn text(v: &Value) -> String {
    match v {
        Value::Text(s) => s.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database whose `readers` is the one an earlier build wrote, with
    /// no `lineage`, holding one replica's record.
    fn old_readers() -> Database {
        let mut db = Database::new();
        for sql in [
            "create collection readers \
             (key text @hash, tenant text @hash, node text @hash, follows text)",
            r#"put readers {key: "acme/u1", tenant: "acme", node: "u1", follows: "http://e1"}"#,
        ] {
            db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
        }
        db
    }

    /// A directory whose `readers` has no `lineage` keeps its records: the
    /// field is added as an alter, and a standby's, following a primary
    /// from before it, selects the fields it has. Asked of it, the select
    /// failed with the field not found, read as an empty directory: every
    /// read replica record forgotten.
    #[test]
    fn a_directory_from_before_readers_kept_lineage_keeps_its_records() {
        let mut d = Directory::load(Arc::new(RwLock::new(old_readers()))).unwrap();
        assert_eq!(d.readers("acme").len(), 1, "the record kept");
        assert_eq!(d.readers("acme")[0].lineage, None);
        d.set_reader("acme", "u1", "http://e2", Some("aaaaaaaa"))
            .unwrap();
        d.reload().unwrap();
        assert_eq!(d.readers("acme")[0].lineage.as_deref(), Some("aaaaaaaa"));

        let mut db = old_readers();
        db.follow(Vec::new()).unwrap();
        let standby = Directory::load(Arc::new(RwLock::new(db))).unwrap();
        assert_eq!(standby.readers("acme").len(), 1, "a standby's kept too");
    }

    /// A field not found in a collection that is there is the error it is,
    /// never an empty collection; only a collection not there yet -- a
    /// standby's, before the primary's first writes -- reads as empty.
    #[test]
    fn a_field_not_found_is_not_an_empty_directory() {
        let db = old_readers();
        let missing = rows(&db, "readers", "get readers select key, lineage");
        assert!(missing.is_err(), "{missing:?}");
        let none = rows(&db, "scope", "get scope select region").unwrap();
        assert!(none.is_empty());
    }

    /// A lineage succeeded keeps the one before among those the tenant was,
    /// reloaded alike; a lineage recorded afresh -- a tenant made again --
    /// keeps none.
    #[test]
    fn a_lineage_succeeded_keeps_the_one_before() {
        let mut d = Directory::in_memory();
        d.set_lineage("acme", Some("aaaaaaaa")).unwrap();
        d.succeed_lineage("acme", "bbbbbbbb").unwrap();
        d.succeed_lineage("acme", "bbbbbbbb").unwrap();
        d.succeed_lineage("acme", "cccccccc").unwrap();
        assert_eq!(d.lineage("acme").as_deref(), Some("cccccccc"));
        assert_eq!(d.former_lineages("acme"), ["aaaaaaaa", "bbbbbbbb"]);
        d.reload().unwrap();
        assert_eq!(d.former_lineages("acme"), ["aaaaaaaa", "bbbbbbbb"]);
        // Back to one it was: that one is no longer among them.
        d.succeed_lineage("acme", "aaaaaaaa").unwrap();
        assert_eq!(d.former_lineages("acme"), ["bbbbbbbb", "cccccccc"]);
        d.set_lineage("acme", Some("dddddddd")).unwrap();
        assert!(d.former_lineages("acme").is_empty());
    }

    /// Lineages are kept in the maps and follow the change ring, as
    /// placements do: a standby's catch up, a tenant gone takes its own.
    #[test]
    fn lineages_follow_the_change_ring() {
        let path = std::env::temp_dir().join(format!("fenec-dir-lin-{}.fenec", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut d = Directory::open(&path).unwrap();
        d.set_node("n1", node("127.0.0.1:1")).unwrap();
        d.place("acme", "n1", State::Active).unwrap();
        d.set_lineage("acme", Some("aaaaaaaa")).unwrap();
        assert_eq!(d.lineage("acme").as_deref(), Some("aaaaaaaa"));
        d.reload().unwrap();
        assert_eq!(d.lineage("acme").as_deref(), Some("aaaaaaaa"));
        // A row written behind the maps, as a standby's follower writes
        // the primary's: caught up from the ring.
        {
            let mut g = write(&d.db);
            g.execute(
                &fenec_ql::parse_one(r#"put lineages {tenant: "beta", lineage: "bbbbbbbb"}"#)
                    .unwrap(),
            )
            .unwrap();
        }
        d.ids.kept = true;
        d.refresh().unwrap();
        assert_eq!(d.lineage("beta").as_deref(), Some("bbbbbbbb"));
        d.remove_tenant("acme").unwrap();
        assert_eq!(d.lineage("acme"), None);
        drop(d);
        let _ = std::fs::remove_file(&path);
    }

    /// The history a tenant's primary forked at its last failover is kept
    /// beside its lineage -- reloaded, and caught up from the ring as a
    /// standby catches up -- and goes where the lineage is succeeded by
    /// another file's, which does not hold it; with no lineage known it is
    /// not kept. A directory whose `lineages` has no `history`, a router's
    /// from before it, takes the field as an alter and keeps its rows.
    #[test]
    fn a_tenant_s_last_failover_history_is_kept_beside_its_lineage() {
        let mut d = Directory::in_memory();
        d.set_history("acme", "00000000000000aa").unwrap();
        assert_eq!(d.history("acme"), None, "kept with no lineage known");
        d.set_lineage("acme", Some("aaaaaaaa")).unwrap();
        d.set_history("acme", "aaaaaaaa000000bb").unwrap();
        assert_eq!(d.history("acme").as_deref(), Some("aaaaaaaa000000bb"));
        d.reload().unwrap();
        assert_eq!(d.history("acme").as_deref(), Some("aaaaaaaa000000bb"));
        assert_eq!(d.lineage("acme").as_deref(), Some("aaaaaaaa"));
        // A promotion's placement and history, in one block.
        d.place_promoted("acme", "n2", Some("aaaaaaaa000000ff"))
            .unwrap();
        d.reload().unwrap();
        assert_eq!(d.placement("acme").map(|p| p.node.as_str()), Some("n2"));
        assert_eq!(d.history("acme").as_deref(), Some("aaaaaaaa000000ff"));
        {
            let mut g = write(&d.db);
            g.execute(
                &fenec_ql::parse_one(
                    r#"put lineages {tenant: "beta", lineage: "bbbbbbbb", history: "bbbbbbbb000000cc"}"#,
                )
                .unwrap(),
            )
            .unwrap();
        }
        d.ids.kept = true;
        d.refresh().unwrap();
        assert_eq!(d.history("beta").as_deref(), Some("bbbbbbbb000000cc"));
        d.succeed_lineage("acme", "dddddddd").unwrap();
        assert_eq!(d.history("acme"), None, "kept past another file's lineage");
        d.reload().unwrap();
        assert_eq!(d.history("acme"), None);

        let mut db = Database::new();
        for sql in [
            "create collection lineages (tenant text @hash, lineage text, was text)",
            r#"put lineages {tenant: "acme", lineage: "aaaaaaaa"}"#,
        ] {
            db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
        }
        let mut d = Directory::load(Arc::new(RwLock::new(db))).unwrap();
        assert_eq!(d.lineage("acme").as_deref(), Some("aaaaaaaa"));
        d.set_history("acme", "aaaaaaaa000000ee").unwrap();
        d.reload().unwrap();
        assert_eq!(d.history("acme").as_deref(), Some("aaaaaaaa000000ee"));
    }

    fn node(addr: &str) -> Node {
        Node {
            addr: addr.into(),
            token: "t".into(),
            ..Node::default()
        }
    }

    #[test]
    fn a_placement_survives_reopening() {
        let path = std::env::temp_dir().join(format!("fenec-dir-{}.fenec", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut d = Directory::open(&path).unwrap();
            d.set_node("n1", node("127.0.0.1:1")).unwrap();
            d.set_node("n2", node("127.0.0.1:2")).unwrap();
            d.place("acme", "n1", State::Active).unwrap();
            d.place("acme", "n2", State::Moving).unwrap();
            d.place("beta", "n1", State::Active).unwrap();
            d.remove_tenant("beta").unwrap();
        }
        let d = Directory::open(&path).unwrap();
        assert_eq!(d.nodes().len(), 2);
        assert_eq!(
            d.placement("acme"),
            Some(&Placement {
                node: "n2".into(),
                state: State::Moving
            })
        );
        assert_eq!(d.placement("beta"), None);
        let _ = std::fs::remove_file(&path);
    }

    /// A tenant's replica is kept with the directory, and goes with the
    /// tenant, or with the node that held it.
    #[test]
    fn a_replica_is_kept_and_goes_with_its_tenant_or_node() {
        let path = std::env::temp_dir().join(format!("fenec-dir-r-{}.fenec", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut d = Directory::open(&path).unwrap();
            for n in ["n1", "n2", "n3"] {
                d.set_node(n, node(n)).unwrap();
            }
            d.place("acme", "n1", State::Active).unwrap();
            d.place("beta", "n2", State::Active).unwrap();
            d.place("gamma", "n1", State::Active).unwrap();
            d.set_replica("acme", Some("n2")).unwrap();
            d.set_replica("beta", Some("n3")).unwrap();
            d.set_replica("gamma", Some("n3")).unwrap();
            d.set_replica("acme", Some("n3")).unwrap();
        }
        let mut d = Directory::open(&path).unwrap();
        assert_eq!(d.replica("acme"), Some("n3"));
        assert_eq!(d.replicas_on("n3"), ["acme", "beta", "gamma"]);
        assert_eq!(d.replicas_by_node()["n2"], 0);
        d.remove_tenant("gamma").unwrap();
        assert_eq!(d.replica("gamma"), None);
        d.remove_tenant("beta").unwrap();
        d.remove_node("n3").unwrap();
        assert_eq!(d.replica("acme"), None);
        let d = Directory::open(&path).unwrap();
        assert_eq!(d.replica("acme"), None);
        assert!(d.replicas_on("n3").is_empty());
        let _ = std::fs::remove_file(&path);
    }

    /// Writes made underneath the directory, as a standby's arrive from its
    /// primary.
    fn underneath(d: &Directory, sql: &str) {
        let mut g = d.db().write().unwrap();
        g.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
    }

    /// The maps, in an order that does not depend on hashing.
    fn maps(d: &Directory) -> String {
        let mut pairs: Vec<String> = d.pairs.iter().map(|(k, v)| format!("{k}>{v}")).collect();
        pairs.sort();
        let mut replicas: Vec<String> =
            d.replicas.iter().map(|(k, v)| format!("{k}@{v}")).collect();
        replicas.sort();
        format!(
            "{:?} {:?} {pairs:?} {replicas:?} {:?} {:?} {:?} {:?}",
            d.nodes(),
            d.tenants(),
            d.homes(),
            d.regions(),
            d.scope(),
            d.all_readers()
        )
    }

    /// Catching up from the change ring leaves the maps a read of them
    /// gives, through puts, changes, a key that changes and deletions in
    /// each collection -- and a write of the router's own, or a ring the
    /// changes outran, reads them again.
    #[test]
    fn catching_up_gives_the_maps_a_reading_gives() {
        let mut d = Directory::in_memory();
        underneath(&d, r#"put nodes {name: "n1", addr: "a1", token: "t"}"#);
        underneath(
            &d,
            r#"put tenants {name: "acme", node: "n1", state: "active"}"#,
        );
        d.refresh().unwrap();
        for sql in [
            r#"put nodes {name: "n2", addr: "a2", token: "t"}"#,
            r#"put tenants {name: "beta", node: "n1", state: "active"}"#,
            r#"put tenants {name: "gamma", node: "n2", state: "moving"}"#,
            r#"set tenants {node: "n2", state: "moving"} where name = "acme""#,
            r#"set tenants {name: "beta2"} where name = "beta""#,
            r#"del tenants where name = "gamma""#,
            r#"put pairs {node: "n1", standby: "n2"}"#,
            r#"set nodes {addr: "a1b"} where name = "n1""#,
            r#"put nodes {name: "n3", addr: "a3", token: "t"}"#,
            r#"del nodes where name = "n3""#,
            r#"put replicas {tenant: "acme", node: "n1"}"#,
            r#"put replicas {tenant: "beta2", node: "n2"}"#,
            r#"set replicas {node: "n2"} where tenant = "acme""#,
            r#"del replicas where tenant = "beta2""#,
            r#"set nodes {region: "eu", zone: "eu-a"} where name = "n1""#,
            r#"put nodes {name: "n4", addr: "a4", token: "t", region: "us"}"#,
            r#"set nodes {zone: null} where name = "n1""#,
            // A tenant leaving for a region keeping its own directory, one
            // made there, and one coming back.
            r#"put regions {name: "us", routers: "http://a:1,http://b:2"}"#,
            r#"set tenants {node: "@us"} where name = "acme""#,
            r#"put tenants {name: "zeta", node: "@us", state: "active"}"#,
            r#"set regions {routers: "http://b:2"} where name = "us""#,
            r#"set tenants {node: "n1"} where name = "zeta""#,
            r#"del tenants where name = "acme""#,
            r#"put scope {region: "us"}"#,
            r#"del scope where region = "us""#,
            r#"del regions where name = "us""#,
            // Read replicas: two of one tenant, one re-pointed, one gone.
            r#"put readers {key: "zeta/n4", tenant: "zeta", node: "n4", follows: "http://a1"}"#,
            r#"put readers {key: "zeta/n2", tenant: "zeta", node: "n2", follows: "http://a1"}"#,
            r#"set readers {follows: "http://a2"} where key = "zeta/n4""#,
            r#"del readers where key = "zeta/n2""#,
        ] {
            underneath(&d, sql);
            assert!(d.stale());
            assert!(d.catch_up().unwrap(), "{sql}");
            let read = Directory::load(Arc::clone(d.db())).unwrap();
            assert_eq!(maps(&d), maps(&read), "{sql}");
        }

        // A write of the router's own keeps the maps, not their ids.
        d.place("delta", "n1", State::Active).unwrap();
        underneath(&d, r#"del tenants where name = "delta""#);
        assert!(!d.catch_up().unwrap());
        d.refresh().unwrap();
        assert_eq!(d.placement("delta"), None);

        // Changes past the ring's reach.
        d.db().write().unwrap().set_change_capacity(2);
        for i in 0..5 {
            underneath(
                &d,
                &format!(r#"put tenants {{name: "t{i}", node: "n2", state: "active"}}"#),
            );
        }
        assert!(!d.catch_up().unwrap());
        d.refresh().unwrap();
        let read = Directory::load(Arc::clone(d.db())).unwrap();
        assert_eq!(maps(&d), maps(&read));
    }

    /// A directory written before nodes had a region and a zone takes the
    /// fields as it opens: its nodes read the unnamed region and zone, and
    /// keep what they are given from then on.
    #[test]
    fn a_directory_from_before_regions_takes_them() {
        let path = std::env::temp_dir().join(format!("fenec-dir-old-{}.fenec", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut db = fenec_core::fs::open(&path).unwrap();
            for sql in [
                "create collection nodes (name text @hash, addr text, token text)",
                r#"put nodes {name: "n1", addr: "a1", token: "t"}"#,
            ] {
                db.execute(&fenec_ql::parse_one(sql).unwrap()).unwrap();
            }
            db.sync().unwrap();
        }
        {
            let mut d = Directory::open(&path).unwrap();
            assert_eq!(d.node("n1"), Some(&node("a1")));
            assert_eq!(d.region_of("n1"), "");
            let mut n2 = node("a2");
            n2.region = "eu".into();
            n2.zone = "eu-b".into();
            d.set_node("n2", n2.clone()).unwrap();
            d.place("acme", "n2", State::Active).unwrap();
            assert_eq!(d.tenant_region("acme"), Some("eu"));
        }
        let d = Directory::open(&path).unwrap();
        assert_eq!(d.node("n1"), Some(&node("a1")));
        assert_eq!(
            d.node("n2").map(|n| (n.region.as_str(), n.zone.as_str())),
            Some(("eu", "eu-b"))
        );
        let _ = std::fs::remove_file(&path);
    }

    /// The copies in their primary's zone, by region: only where both zones
    /// are named and the same.
    #[test]
    fn a_copy_is_in_its_primarys_zone_only_where_both_are_named() {
        let mut d = Directory::in_memory();
        for (name, region, zone) in [
            ("e1", "eu", "a"),
            ("e2", "eu", "a"),
            ("e3", "eu", "b"),
            ("u1", "us", ""),
            ("u2", "us", ""),
        ] {
            let mut n = node(name);
            n.region = region.into();
            n.zone = zone.into();
            d.set_node(name, n).unwrap();
        }
        d.place("t1", "e1", State::Active).unwrap();
        d.place("t2", "e1", State::Active).unwrap();
        d.place("t3", "u1", State::Active).unwrap();
        d.set_replica("t1", Some("e2")).unwrap();
        d.set_replica("t2", Some("e3")).unwrap();
        d.set_replica("t3", Some("u2")).unwrap();
        let by = d.same_zone_copies();
        assert_eq!((by["eu"], by["us"]), (1, 0));
        // A pair's standby is a copy too.
        d.place("t4", "e3", State::Active).unwrap();
        d.set_node("e4", {
            let mut n = node("e4");
            n.region = "eu".into();
            n.zone = "b".into();
            n
        })
        .unwrap();
        d.set_standby("e3", Some("e4")).unwrap();
        assert_eq!(d.copy_of("t4"), Some("e4"));
        assert_eq!(d.same_zone_copies()["eu"], 2);
        // A tenant, a replica, either side of a pair: each holds its node's
        // region in place.
        for held in ["e1", "e2", "e3", "e4"] {
            assert!(d.holds(held), "{held}");
        }
        d.set_node("e5", node("e5")).unwrap();
        assert!(!d.holds("e5"));
    }

    /// A region's own directory names its region, and opens for that
    /// region's router alone; the global directory, holding nodes and
    /// naming none, for no region's router.
    #[test]
    fn a_directory_opens_for_the_router_of_what_it_holds() {
        let mut fresh = Directory::in_memory();
        fresh.settle_scope(Some("us")).unwrap();
        assert_eq!(fresh.scope(), Some("us"));
        fresh.settle_scope(Some("us")).unwrap();
        let other = fresh.settle_scope(Some("eu")).unwrap_err();
        assert!(other.contains("region `us`'s own"), "{other}");
        let global = fresh.settle_scope(None).unwrap_err();
        assert!(global.contains("--region us"), "{global}");

        let mut global = Directory::in_memory();
        global.settle_scope(None).unwrap();
        global.set_node("n1", node("a")).unwrap();
        let refused = global.settle_scope(Some("us")).unwrap_err();
        assert!(refused.contains("global directory"), "{refused}");
        assert_eq!(global.scope(), None);

        // A home is a tenant of the directory, and no placement.
        global.place_home("acme", "us").unwrap();
        assert!(global.exists("acme") && global.placement("acme").is_none());
        assert_eq!(global.tenant_region("acme"), Some("us"));
        assert!(global.primaries()["n1"].is_empty());
        global.place("acme", "n1", State::Active).unwrap();
        assert_eq!(global.home("acme"), None);
        assert_eq!(global.primaries()["n1"], ["acme"]);
    }

    /// A tenant's read replicas are kept with the directory, one a node,
    /// and go with the node that held them; a node holding one holds its
    /// region in place.
    #[test]
    fn a_read_replica_is_kept_and_goes_with_its_node() {
        let path = std::env::temp_dir().join(format!("fenec-dir-rr-{}.fenec", std::process::id()));
        let _ = std::fs::remove_file(&path);
        {
            let mut d = Directory::open(&path).unwrap();
            for (n, region) in [("e1", "eu"), ("u1", "us"), ("a1", "ap")] {
                let mut nd = node(n);
                nd.region = region.into();
                d.set_node(n, nd).unwrap();
            }
            d.place("acme", "e1", State::Active).unwrap();
            d.place("beta", "e1", State::Active).unwrap();
            d.set_reader("acme", "u1", "http://e1", Some("aaaaaaaa"))
                .unwrap();
            d.set_reader("acme", "a1", "http://e1", None).unwrap();
            d.set_reader("beta", "u1", "http://e1", None).unwrap();
            d.set_reader("acme", "u1", "http://e2", Some("aaaaaaaa"))
                .unwrap();
            assert!(d.holds("u1") && d.holds("a1"));
        }
        let mut d = Directory::open(&path).unwrap();
        assert_eq!(
            d.reader_in("acme", "us"),
            Some(&Reader {
                node: "u1".into(),
                follows: "http://e2".into(),
                lineage: Some("aaaaaaaa".into()),
            })
        );
        assert_eq!(d.readers("acme").len(), 2);
        assert_eq!(d.reader_in("acme", "eu"), None);
        assert_eq!(d.readers_by_node()["u1"], 2);
        // A tenant forgotten keeps them -- the router takes them off with
        // their files -- and a node holding one is not removed until it is
        // taken off, or a copy would follow on unrecorded.
        d.remove_tenant("beta").unwrap();
        assert_eq!(d.readers("beta").len(), 1);
        d.remove_reader("beta", "u1").unwrap();
        assert!(d.readers("beta").is_empty());
        let refused = d.remove_node("a1").unwrap_err().to_string();
        assert!(refused.contains("read replica of `acme`"), "{refused}");
        assert!(d.node("a1").is_some());
        d.remove_reader("acme", "a1").unwrap();
        d.remove_node("a1").unwrap();
        assert_eq!(d.reader_in("acme", "ap"), None);
        d.remove_reader("acme", "u1").unwrap();
        assert!(!d.holds("u1"));
        let d = Directory::open(&path).unwrap();
        assert!(d.all_readers().is_empty());
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_node_holding_tenants_is_not_removed() {
        let mut d = Directory::in_memory();
        d.set_node("n1", node("a")).unwrap();
        d.place("acme", "n1", State::Active).unwrap();
        assert!(d.remove_node("n1").is_err());
        d.remove_tenant("acme").unwrap();
        d.remove_node("n1").unwrap();
        assert!(d.nodes().is_empty());
    }
}
