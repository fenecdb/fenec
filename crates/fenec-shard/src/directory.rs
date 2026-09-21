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

const SCHEMA: [&str; 2] = [
    "create collection if not exists nodes (name text @hash, addr text, token text)",
    "create collection if not exists tenants (name text @hash, node text @hash, state text)",
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

pub struct Directory {
    db: Database,
    nodes: BTreeMap<String, Node>,
    tenants: HashMap<String, Placement>,
}

impl Directory {
    /// Opens (or creates) the directory file.
    pub fn open(path: impl AsRef<Path>) -> Result<Directory> {
        Directory::load(fenec_core::fs::open(path)?)
    }

    /// A directory that lives only as long as the process: tests.
    pub fn in_memory() -> Directory {
        Directory::load(Database::new()).expect("an empty database loads")
    }

    fn load(mut db: Database) -> Result<Directory> {
        for sql in SCHEMA {
            db.execute(&fenec_ql::parse_one(sql)?)?;
        }
        let mut nodes = BTreeMap::new();
        for row in rows(&db, "get nodes select name, addr, token")? {
            nodes.insert(
                text(&row[0]),
                Node {
                    addr: text(&row[1]),
                    token: text(&row[2]),
                },
            );
        }
        let mut tenants = HashMap::new();
        for row in rows(&db, "get tenants select name, node, state")? {
            tenants.insert(
                text(&row[0]),
                Placement {
                    node: text(&row[1]),
                    state: State::parse(&text(&row[2])),
                },
            );
        }
        Ok(Directory { db, nodes, tenants })
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
        self.db.execute_with(&stmt, params)?;
        self.db.sync()
    }
}

fn rows(db: &Database, sql: &str) -> Result<Vec<Vec<Value>>> {
    match db.query(&fenec_ql::parse_one(sql)?, &[])? {
        Response::Rows(rs) => Ok(rs.rows.into_iter().map(|r| r.values).collect()),
        _ => Ok(Vec::new()),
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
