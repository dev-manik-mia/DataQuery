//! Shell calls as database commands. Every statement becomes one command
//! document (`find`, `aggregate`, `insert`, `update`, …) so it runs the same
//! way inside or outside a transaction, carries a `comment` for cancelling,
//! and can be wrapped in `explain`.

use dq_core::{DbError, DbResult};
use mongodb::bson::{Bson, Document, doc, oid::ObjectId};

use crate::shell::Call;

/// How the reply becomes a result.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// A cursor command: rows come from the cursor.
    Cursor,
    /// The reply itself as one row (without `ok` and cluster fields).
    Reply,
    /// One field of the reply as a one-row result named `name`.
    Field { field: &'static str, name: &'static str },
    /// The `cursor.firstBatch` of a single-batch command.
    FirstBatch,
    /// `countDocuments`: the first batch's `n`, or 0.
    CountDocuments,
    /// `distinct`: one row per value.
    Distinct,
    /// `listDatabases`: one row per database.
    Databases,
    /// `findAndModify`: the returned document, if any.
    FindAndModify,
    Insert { ids: Vec<Bson> },
    Update,
    Delete,
    /// A command whose success is the whole result.
    Done,
}

/// A command to run, on which database, and how to read its reply.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub database: String,
    pub command: Document,
    pub shape: Shape,
}

impl Plan {
    /// Read commands that `explain` accepts.
    pub fn explainable(&self) -> bool {
        self.command
            .keys()
            .next()
            .is_some_and(|k| matches!(k.as_str(), "find" | "aggregate" | "count" | "distinct" | "findAndModify" | "update" | "delete"))
    }
}

fn err(message: impl AsRef<str>) -> DbError {
    DbError::new(dq_core::ErrorKind::Syntax, message)
}

fn doc_arg(args: &[Bson], i: usize, method: &str, what: &str) -> DbResult<Option<Document>> {
    match args.get(i) {
        None | Some(Bson::Null | Bson::Undefined) => Ok(None),
        Some(Bson::Document(d)) => Ok(Some(d.clone())),
        Some(_) => Err(err(format!("{method}(): the {what} must be an object."))),
    }
}

fn required_doc(args: &[Bson], i: usize, method: &str, what: &str) -> DbResult<Document> {
    doc_arg(args, i, method, what)?.ok_or_else(|| err(format!("{method}() needs the {what}.")))
}

fn number_arg(args: &[Bson], method: &str) -> DbResult<i64> {
    match args.first() {
        Some(Bson::Int32(v)) => Ok(i64::from(*v)),
        Some(Bson::Int64(v)) => Ok(*v),
        Some(Bson::Double(v)) if v.fract() == 0.0 => Ok(*v as i64),
        _ => Err(err(format!(".{method}() needs a whole number."))),
    }
}

/// True when a document has an update operator (`$set`, …) at the top.
fn is_update_document(d: &Document) -> bool {
    d.keys().next().is_some_and(|k| k.starts_with('$'))
}

/// Copies allowed option names from `options` into `target`.
fn copy_options(target: &mut Document, options: Option<Document>, allowed: &[&str]) -> DbResult<()> {
    for (k, v) in options.unwrap_or_default() {
        if allowed.contains(&k.as_str()) {
            target.insert(k, v);
        } else {
            return Err(err(format!("The option `{k}` is not supported here.")));
        }
    }
    Ok(())
}

/// `{ status: 1, createdAt: -1 }` → `status_1_createdAt_-1`, as the server names indexes.
pub fn index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(k, v)| {
            let v = match v {
                Bson::String(s) => s.clone(),
                Bson::Int32(n) => n.to_string(),
                Bson::Int64(n) => n.to_string(),
                Bson::Double(n) => crate::literal::format_double(*n),
                other => other.to_string(),
            };
            format!("{k}_{v}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

fn index_spec(keys: Document, options: Option<Document>) -> Document {
    let mut spec = options.unwrap_or_default();
    if !spec.contains_key("name") {
        spec.insert("name", index_name(&keys));
    }
    spec.insert("key", keys);
    spec
}

/// Builds the command for a call. `comment` tags it for cancelling;
/// `max_time_ms` is the statement timeout for reads.
pub fn plan(call: &Call, database: &str, comment: &str, max_time_ms: Option<u64>) -> DbResult<Plan> {
    let database = call.database.clone().unwrap_or_else(|| database.to_owned());
    let mut plan = match &call.collection {
        Some(collection) => collection_plan(call, collection, &database)?,
        None => database_plan(call, &database)?,
    };
    if let Some(explain_args) = call.chained("explain") {
        let verbosity = explain_args.first().cloned();
        if !plan.explainable() {
            return Err(err(format!("{}() cannot be explained.", call.method)));
        }
        let verbosity = match verbosity {
            None | Some(Bson::Boolean(false)) => "queryPlanner".to_owned(),
            Some(Bson::Boolean(true)) => "allPlansExecution".to_owned(),
            Some(Bson::String(v)) => v,
            Some(_) => return Err(err(".explain() takes a verbosity such as 'executionStats'.")),
        };
        plan = Plan {
            database: plan.database,
            command: doc! { "explain": plan.command, "verbosity": verbosity },
            shape: Shape::Reply,
        };
    }
    let first = plan.command.keys().next().cloned().unwrap_or_default();
    let commented = matches!(
        first.as_str(),
        "find" | "aggregate" | "count" | "distinct" | "insert" | "update" | "delete" | "findAndModify" | "createIndexes"
            | "explain"
    );
    if commented && !plan.command.contains_key("comment") {
        plan.command.insert("comment", comment);
    }
    let reads = matches!(first.as_str(), "find" | "aggregate" | "count" | "distinct" | "findAndModify" | "explain");
    if let Some(ms) = max_time_ms.filter(|ms| *ms > 0)
        && reads
        && !plan.command.contains_key("maxTimeMS")
    {
        plan.command.insert("maxTimeMS", i64::try_from(ms).unwrap_or(i64::MAX));
    }
    Ok(plan)
}

fn collection_plan(call: &Call, coll: &str, database: &str) -> DbResult<Plan> {
    let args = &call.args;
    let m = call.method.as_str();
    let at = |command: Document, shape: Shape| Plan { database: database.to_owned(), command, shape };
    Ok(match m {
        "find" | "findOne" => {
            let mut c = doc! { "find": coll };
            if let Some(filter) = doc_arg(args, 0, m, "filter")? {
                c.insert("filter", filter);
            }
            if let Some(projection) = doc_arg(args, 1, m, "projection")? {
                c.insert("projection", projection);
            }
            let mut count = false;
            for (name, a) in &call.chain {
                match name.as_str() {
                    "sort" => _ = c.insert("sort", required_doc(a, 0, "sort", "sort order")?),
                    "project" | "projection" => _ = c.insert("projection", required_doc(a, 0, name, "projection")?),
                    "limit" => _ = c.insert("limit", number_arg(a, name)?),
                    "skip" => _ = c.insert("skip", number_arg(a, name)?),
                    "batchSize" => _ = c.insert("batchSize", number_arg(a, name)?),
                    "maxTimeMS" => _ = c.insert("maxTimeMS", number_arg(a, name)?),
                    "hint" | "collation" | "min" | "max" | "comment" => {
                        let v = a.first().cloned().ok_or_else(|| err(format!(".{name}() needs a value.")))?;
                        c.insert(name.clone(), v);
                    }
                    "allowDiskUse" => _ = c.insert("allowDiskUse", true),
                    "count" | "itcount" | "size" => count = true,
                    "toArray" | "pretty" | "explain" => {}
                    other => return Err(err(format!(".{other}() is not supported after find()."))),
                }
            }
            if count {
                let mut n = doc! { "count": coll };
                for (from, to) in [("filter", "query"), ("skip", "skip"), ("limit", "limit"), ("hint", "hint")] {
                    if let Some(v) = c.get(from) {
                        n.insert(to, v.clone());
                    }
                }
                return Ok(at(n, Shape::Field { field: "n", name: "count" }));
            }
            if m == "findOne" {
                c.insert("limit", 1);
                c.insert("singleBatch", true);
                return Ok(at(c, Shape::FirstBatch));
            }
            at(c, Shape::Cursor)
        }
        "aggregate" => {
            let pipeline = match args.first() {
                Some(Bson::Array(stages)) => stages.clone(),
                Some(Bson::Document(_)) => args.iter().take_while(|a| matches!(a, Bson::Document(_))).cloned().collect(),
                None => Vec::new(),
                Some(_) => return Err(err("aggregate() needs a pipeline: an array of stages.")),
            };
            let mut c = doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} };
            let options = if matches!(args.first(), Some(Bson::Array(_))) { doc_arg(args, 1, m, "options")? } else { None };
            copy_options(&mut c, options, &["allowDiskUse", "collation", "hint", "let", "maxTimeMS", "comment", "bypassDocumentValidation"])?;
            at(c, Shape::Cursor)
        }
        "countDocuments" => {
            let mut pipeline = vec![Bson::Document(doc! { "$match": doc_arg(args, 0, m, "filter")?.unwrap_or_default() })];
            let options = doc_arg(args, 1, m, "options")?.unwrap_or_default();
            for key in ["skip", "limit"] {
                if let Some(v) = options.get(key) {
                    pipeline.push(Bson::Document(doc! { format!("${key}"): v.clone() }));
                }
            }
            pipeline.push(Bson::Document(doc! { "$group": { "_id": 1, "n": { "$sum": 1 } } }));
            at(doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} }, Shape::CountDocuments)
        }
        "estimatedDocumentCount" => at(doc! { "count": coll }, Shape::Field { field: "n", name: "count" }),
        "count" => {
            let mut c = doc! { "count": coll };
            if let Some(query) = doc_arg(args, 0, m, "filter")? {
                c.insert("query", query);
            }
            at(c, Shape::Field { field: "n", name: "count" })
        }
        "distinct" => {
            let Some(Bson::String(key)) = args.first() else {
                return Err(err("distinct() needs a field name in quotes."));
            };
            let mut c = doc! { "distinct": coll, "key": key };
            if let Some(query) = doc_arg(args, 1, m, "filter")? {
                c.insert("query", query);
            }
            at(c, Shape::Distinct)
        }
        "insertOne" | "insert" | "insertMany" => {
            let docs: Vec<Document> = match args.first() {
                Some(Bson::Document(d)) if m != "insertMany" => vec![d.clone()],
                Some(Bson::Array(items)) if m != "insertOne" => items
                    .iter()
                    .map(|i| i.as_document().cloned().ok_or_else(|| err("Every inserted document must be an object.")))
                    .collect::<DbResult<_>>()?,
                _ => return Err(err(format!("{m}() needs {}.", if m == "insertMany" { "an array of documents" } else { "a document" }))),
            };
            if docs.is_empty() {
                return Err(err("There is nothing to insert."));
            }
            let mut ids = Vec::with_capacity(docs.len());
            let docs: Vec<Bson> = docs
                .into_iter()
                .map(|mut d| {
                    if !d.contains_key("_id") {
                        // Like the shell: the client assigns the id, so it can be reported.
                        let mut with_id = doc! { "_id": ObjectId::new() };
                        with_id.extend(d);
                        d = with_id;
                    }
                    ids.push(d.get("_id").cloned().unwrap_or(Bson::Null));
                    Bson::Document(d)
                })
                .collect();
            let mut c = doc! { "insert": coll, "documents": docs };
            copy_options(&mut c, doc_arg(args, 1, m, "options")?, &["ordered", "bypassDocumentValidation", "writeConcern"])?;
            at(c, Shape::Insert { ids })
        }
        "updateOne" | "updateMany" | "update" | "replaceOne" => {
            let filter = required_doc(args, 0, m, "filter")?;
            let update = match args.get(1) {
                Some(Bson::Document(d)) => {
                    let operators = is_update_document(d);
                    if m == "replaceOne" && operators {
                        return Err(err("replaceOne() takes a whole document without update operators; use updateOne() for $set."));
                    }
                    if m != "replaceOne" && !operators {
                        return Err(err(format!("{m}() needs update operators such as {{ $set: {{ … }} }}; use replaceOne() to replace a document.")));
                    }
                    Bson::Document(d.clone())
                }
                Some(Bson::Array(stages)) if m != "replaceOne" => Bson::Array(stages.clone()),
                _ => return Err(err(format!("{m}() needs an update document."))),
            };
            let options = doc_arg(args, 2, m, "options")?.unwrap_or_default();
            let multi = m == "updateMany" || (m == "update" && options.get_bool("multi").unwrap_or(false));
            let mut entry = doc! { "q": filter, "u": update, "multi": multi };
            for (k, v) in options {
                match k.as_str() {
                    "upsert" | "arrayFilters" | "hint" | "collation" | "let" => _ = entry.insert(k, v),
                    "multi" => {}
                    other => return Err(err(format!("The option `{other}` is not supported here."))),
                }
            }
            at(doc! { "update": coll, "updates": [entry] }, Shape::Update)
        }
        "deleteOne" | "deleteMany" | "remove" => {
            let filter = required_doc(args, 0, m, "filter")?;
            let options = doc_arg(args, 1, m, "options")?.unwrap_or_default();
            let mut entry = doc! { "q": filter, "limit": i32::from(m == "deleteOne") };
            copy_options(&mut entry, Some(options), &["hint", "collation"])?;
            at(doc! { "delete": coll, "deletes": [entry] }, Shape::Delete)
        }
        "findOneAndUpdate" | "findOneAndReplace" | "findOneAndDelete" => {
            let mut c = doc! { "findAndModify": coll, "query": required_doc(args, 0, m, "filter")? };
            let options_at = if m == "findOneAndDelete" { 1 } else { 2 };
            if m == "findOneAndDelete" {
                c.insert("remove", true);
            } else {
                let update = args.get(1).cloned().ok_or_else(|| err(format!("{m}() needs an update.")))?;
                if let Bson::Document(d) = &update
                    && (m == "findOneAndReplace") == is_update_document(d)
                {
                    return Err(err(if m == "findOneAndReplace" {
                        "findOneAndReplace() takes a whole document without update operators."
                    } else {
                        "findOneAndUpdate() needs update operators such as { $set: { … } }."
                    }));
                }
                c.insert("update", update);
            }
            for (k, v) in doc_arg(args, options_at, m, "options")?.unwrap_or_default() {
                match k.as_str() {
                    "projection" => _ = c.insert("fields", v),
                    "returnNewDocument" => _ = c.insert("new", v),
                    "returnDocument" => _ = c.insert("new", v.as_str() == Some("after")),
                    "sort" | "upsert" | "arrayFilters" | "hint" | "collation" | "bypassDocumentValidation" | "let" => {
                        _ = c.insert(k, v);
                    }
                    other => return Err(err(format!("The option `{other}` is not supported here."))),
                }
            }
            at(c, Shape::FindAndModify)
        }
        "createIndex" => {
            let keys = required_doc(args, 0, m, "index keys")?;
            at(doc! { "createIndexes": coll, "indexes": [index_spec(keys, doc_arg(args, 1, m, "options")?)] }, Shape::Done)
        }
        "createIndexes" => {
            let Some(Bson::Array(list)) = args.first() else {
                return Err(err("createIndexes() needs an array of index keys."));
            };
            let options = doc_arg(args, 1, m, "options")?;
            let indexes: Vec<Bson> = list
                .iter()
                .map(|k| {
                    k.as_document()
                        .map(|k| Bson::Document(index_spec(k.clone(), options.clone())))
                        .ok_or_else(|| err("Every index key must be an object."))
                })
                .collect::<DbResult<_>>()?;
            at(doc! { "createIndexes": coll, "indexes": indexes }, Shape::Done)
        }
        "dropIndex" | "dropIndexes" => {
            let index = match args.first() {
                None if m == "dropIndexes" => Bson::String("*".into()),
                Some(v @ (Bson::String(_) | Bson::Document(_) | Bson::Array(_))) => v.clone(),
                _ => return Err(err(format!("{m}() needs an index name or key pattern."))),
            };
            at(doc! { "dropIndexes": coll, "index": index }, Shape::Done)
        }
        "getIndexes" | "indexes" | "getIndexKeys" => at(doc! { "listIndexes": coll, "cursor": {} }, Shape::Cursor),
        "drop" => at(doc! { "drop": coll }, Shape::Done),
        "renameCollection" => {
            let Some(Bson::String(to)) = args.first() else {
                return Err(err("renameCollection() needs the new name in quotes."));
            };
            let drop_target = matches!(args.get(1), Some(Bson::Boolean(true)));
            Plan {
                database: "admin".into(),
                command: doc! {
                    "renameCollection": format!("{database}.{coll}"),
                    "to": format!("{database}.{to}"),
                    "dropTarget": drop_target,
                },
                shape: Shape::Done,
            }
        }
        "stats" | "dataSize" | "storageSize" | "totalIndexSize" => at(
            doc! { "aggregate": coll, "pipeline": [{ "$collStats": { "storageStats": {} } }], "cursor": {} },
            Shape::Cursor,
        ),
        "validate" => at(doc! { "validate": coll }, Shape::Reply),
        "hideIndex" | "unhideIndex" => {
            let index = args.first().cloned().ok_or_else(|| err(format!("{m}() needs an index name.")))?;
            let key = if matches!(index, Bson::String(_)) { "name" } else { "keyPattern" };
            at(
                doc! { "collMod": coll, "index": { key: index, "hidden": m == "hideIndex" } },
                Shape::Reply,
            )
        }
        other => return Err(err(format!("`{other}()` is not supported on a collection."))),
    })
}

fn database_plan(call: &Call, database: &str) -> DbResult<Plan> {
    let args = &call.args;
    let m = call.method.as_str();
    let at = |command: Document, shape: Shape| Plan { database: database.to_owned(), command, shape };
    Ok(match m {
        "runCommand" | "adminCommand" => {
            let command = match args.first() {
                Some(Bson::Document(d)) if !d.is_empty() => d.clone(),
                Some(Bson::String(name)) => doc! { name: 1 },
                _ => return Err(err(format!("{m}() needs a command such as {{ ping: 1 }}."))),
            };
            let shape = if command.contains_key("cursor") { Shape::FirstBatch } else { Shape::Reply };
            Plan { database: if m == "adminCommand" { "admin".into() } else { database.to_owned() }, command, shape }
        }
        "getCollectionNames" => at(doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true }, Shape::Cursor),
        "getCollectionInfos" | "listCollections" => {
            let mut c = doc! { "listCollections": 1, "authorizedCollections": true };
            if let Some(filter) = doc_arg(args, 0, m, "filter")? {
                c.insert("filter", filter);
            }
            at(c, Shape::Cursor)
        }
        "stats" => at(doc! { "dbStats": 1 }, Shape::Reply),
        "serverStatus" => at(doc! { "serverStatus": 1 }, Shape::Reply),
        "hostInfo" => at(doc! { "hostInfo": 1 }, Shape::Reply),
        "hello" => at(doc! { "hello": 1 }, Shape::Reply),
        "isMaster" => at(doc! { "isMaster": 1 }, Shape::Reply),
        "currentOp" => Plan { database: "admin".into(), command: doc! { "currentOp": 1 }, shape: Shape::Reply },
        "getUsers" => at(doc! { "usersInfo": 1 }, Shape::Reply),
        "getRoles" => at(doc! { "rolesInfo": 1 }, Shape::Reply),
        "version" => at(doc! { "buildInfo": 1 }, Shape::Field { field: "version", name: "version" }),
        "getName" => at(doc! { "dbStats": 1, "scale": 1 }, Shape::Field { field: "db", name: "name" }),
        "createCollection" => {
            let Some(Bson::String(name)) = args.first() else {
                return Err(err("createCollection() needs a name in quotes."));
            };
            let mut c = doc! { "create": name };
            c.extend(doc_arg(args, 1, m, "options")?.unwrap_or_default());
            at(c, Shape::Done)
        }
        "createView" => {
            let (Some(Bson::String(name)), Some(Bson::String(source))) = (args.first(), args.get(1)) else {
                return Err(err("createView() needs a name, a source collection and a pipeline."));
            };
            let pipeline = match args.get(2) {
                Some(Bson::Array(p)) => p.clone(),
                None => Vec::new(),
                Some(_) => return Err(err("createView(): the pipeline must be an array.")),
            };
            let mut c = doc! { "create": name, "viewOn": source, "pipeline": pipeline };
            c.extend(doc_arg(args, 3, m, "options")?.unwrap_or_default());
            at(c, Shape::Done)
        }
        "dropDatabase" => at(doc! { "dropDatabase": 1 }, Shape::Done),
        other => return Err(err(format!("`db.{other}()` is not supported. Name a collection, e.g. db.orders.find()."))),
    })
}

/// `show dbs` / `show collections`.
pub fn show(what: &str, database: &str) -> Plan {
    if what == "dbs" {
        Plan { database: "admin".into(), command: doc! { "listDatabases": 1, "nameOnly": false }, shape: Shape::Databases }
    } else {
        Plan {
            database: database.to_owned(),
            command: doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true },
            shape: Shape::Cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::{Statement, parse};

    fn plan_of(text: &str) -> Plan {
        let Statement::Call(call) = parse(text).unwrap() else { panic!() };
        plan(&call, "shop", "c1", Some(5000)).unwrap()
    }

    #[test]
    fn find_with_chain() {
        let p = plan_of("db.orders.find({ status: 'paid' }).sort({ total: -1 }).skip(2).limit(5)");
        assert_eq!(p.shape, Shape::Cursor);
        assert_eq!(
            p.command,
            doc! { "find": "orders", "filter": { "status": "paid" }, "sort": { "total": -1 }, "skip": 2_i64, "limit": 5_i64,
                   "comment": "c1", "maxTimeMS": 5000_i64 }
        );
        let p = plan_of("db.orders.find({ a: 1 }).count()");
        assert_eq!(p.command.get_str("count").unwrap(), "orders");
        let p = plan_of("db.orders.find().explain('executionStats')");
        assert_eq!(p.command.get_document("explain").unwrap().get_str("find").unwrap(), "orders");
    }

    #[test]
    fn writes() {
        let p = plan_of("db.a.insertOne({ x: 1 })");
        let Shape::Insert { ids } = &p.shape else { panic!() };
        assert!(matches!(ids[0], Bson::ObjectId(_)));
        let p = plan_of("db.a.updateMany({}, { $set: { y: 1 } }, { upsert: true })");
        let u = p.command.get_array("updates").unwrap()[0].as_document().unwrap().clone();
        assert_eq!((u.get_bool("multi").unwrap(), u.get_bool("upsert").unwrap()), (true, true));
        assert!(plan_of_err("db.a.updateOne({}, { y: 1 })").contains("update operators"));
        assert!(plan_of_err("db.a.replaceOne({}, { $set: { y: 1 } })").contains("without update operators"));
        let p = plan_of("db.a.createIndex({ status: 1, createdAt: -1 }, { unique: true })");
        let spec = p.command.get_array("indexes").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(spec.get_str("name").unwrap(), "status_1_createdAt_-1");
        let p = plan_of("db.getSiblingDB('x').a.renameCollection('b')");
        assert_eq!((p.database.as_str(), p.command.get_str("to").unwrap()), ("admin", "x.b"));
    }

    fn plan_of_err(text: &str) -> String {
        let Statement::Call(call) = parse(text).unwrap() else { panic!() };
        plan(&call, "shop", "c1", None).unwrap_err().message
    }
}
