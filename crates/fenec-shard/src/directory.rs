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

use fenec_core::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::{Arc, RwLock};

const SCHEMA: [&str; 3] = [
    "create collection if not exists nodes (name text @hash, addr text, token text)",
    "create collection if not exists tenants (name text @hash, node text @hash, state text)",
    // Which node replicates which. A collection of its own rather than a
    // field on `nodes`: a directory written before this existed opens as it
    // is, and the engine has no schema migration to add one.
    "create collection if not exists pairs (node text @hash, standby text)",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    pub addr: String,
    /// The node's `--admin-token`. Never leaves the router.
    pub token: String,
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
    pairs: Pairs,
    /// Whether the maps exist: a standby's arrive with the primary's first
    /// writes, and until then it knows of no tenant rather than that there
    /// is none.
    arrived: bool,
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
            }
        }
        let mut d = Directory {
            db,
            seq: 0,
            adopted: 0,
            nodes: BTreeMap::new(),
            tenants: HashMap::new(),
            pairs: Pairs::new(),
            arrived: false,
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

    /// Reads the maps again when the database has moved.
    pub fn refresh(&mut self) -> Result<()> {
        if self.stale() {
            self.reload()?;
        }
        Ok(())
    }

    fn reload(&mut self) -> Result<()> {
        let g = read(&self.db);
        let mut nodes = BTreeMap::new();
        for row in rows(&g, "get nodes select name, addr, token")? {
            nodes.insert(
                text(&row[0]),
                Node {
                    addr: text(&row[1]),
                    token: text(&row[2]),
                },
            );
        }
        let mut tenants = HashMap::new();
        for row in rows(&g, "get tenants select name, node, state")? {
            tenants.insert(
                text(&row[0]),
                Placement {
                    node: text(&row[1]),
                    state: State::parse(&text(&row[2])),
                },
            );
        }
        let mut pairs = Pairs::new();
        for row in rows(&g, "get pairs select node, standby")? {
            pairs.insert(text(&row[0]), text(&row[1]));
        }
        self.seq = g.change_seq();
        self.adopted = g.adoptions();
        self.nodes = nodes;
        self.tenants = tenants;
        self.pairs = pairs;
        self.arrived = g.collection("tenants").is_ok();
        Ok(())
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

    pub fn nodes(&self) -> &BTreeMap<String, Node> {
        &self.nodes
    }

    pub fn node(&self, name: &str) -> Option<&Node> {
        self.nodes.get(name)
    }

    pub fn placement(&self, tenant: &str) -> Option<&Placement> {
        self.tenants.get(tenant)
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

    /// Adds a node or changes its address and token.
    pub fn set_node(&mut self, name: &str, node: Node) -> Result<()> {
        let params = [
            Value::Text(name.into()),
            Value::Text(node.addr.clone()),
            Value::Text(node.token.clone()),
        ];
        if self.nodes.contains_key(name) {
            self.run("set nodes {addr: $2, token: $3} where name = $1", &params)?;
        } else {
            self.run("put nodes {name: $1, addr: $2, token: $3}", &params)?;
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
        Ok(())
    }

    /// Records where a tenant lives, and in what state.
    pub fn place(&mut self, tenant: &str, node: &str, state: State) -> Result<()> {
        let params = [
            Value::Text(tenant.into()),
            Value::Text(node.into()),
            Value::Text(state.as_str().into()),
        ];
        if self.tenants.contains_key(tenant) {
            self.run("set tenants {node: $2, state: $3} where name = $1", &params)?;
        } else {
            self.run("put tenants {name: $1, node: $2, state: $3}", &params)?;
        }
        self.tenants.insert(
            tenant.into(),
            Placement {
                node: node.into(),
                state,
            },
        );
        Ok(())
    }

    pub fn remove_tenant(&mut self, tenant: &str) -> Result<()> {
        self.run("del tenants where name = $1", &[Value::Text(tenant.into())])?;
        self.tenants.remove(tenant);
        Ok(())
    }

    /// One statement, then a sync: a directory change the router has
    /// acknowledged is on disk.
    fn run(&mut self, sql: &str, params: &[Value]) -> Result<()> {
        let stmt = fenec_ql::parse_one(sql)?;
        let mut g = write(&self.db);
        g.execute_with(&stmt, params)?;
        g.sync()?;
        // The maps are updated by the caller; the counter moved here.
        self.seq = g.change_seq();
        self.adopted = g.adoptions();
        Ok(())
    }
}

fn read(db: &Arc<RwLock<Database>>) -> std::sync::RwLockReadGuard<'_, Database> {
    db.read().unwrap_or_else(|e| e.into_inner())
}

fn write(db: &Arc<RwLock<Database>>) -> std::sync::RwLockWriteGuard<'_, Database> {
    db.write().unwrap_or_else(|e| e.into_inner())
}

fn rows(db: &Database, sql: &str) -> Result<Vec<Vec<Value>>> {
    match db.query(&fenec_ql::parse_one(sql)?, &[]) {
        Ok(Response::Rows(rs)) => Ok(rs.rows.into_iter().map(|r| r.values).collect()),
        Ok(_) => Ok(Vec::new()),
        // A standby's collections arrive with the primary's first writes.
        // Until they do the directory is empty, not broken.
        Err(Error::NotFound(_)) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
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

    fn node(addr: &str) -> Node {
        Node {
            addr: addr.into(),
            token: "t".into(),
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
