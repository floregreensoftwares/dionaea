//! Erasing a user from every table that holds an identifier.
//!
//! Erasure has two steps in ClickHouse. A lightweight `DELETE` marks rows in
//! a hidden column, so every query stops returning them within moments but
//! the data is still in the parts. `ALTER TABLE ... APPLY DELETED MASK`
//! rewrites the parts without those rows, which removes the data from disk.
//! [`Store::count_user_rows`] can count with or without the marked rows, so
//! both steps are verified by a direct count.
//!
//! [`IDENTIFIER_TABLES`] is the one list of tables that hold an anonymous or
//! user id. A test over the migrations fails when a table with such a column
//! is missing from it, so a new table cannot escape erasure.

use crate::{Error, Store, http};

/// A table that holds identifiers, and which columns hold them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdentifierTable {
    pub name: &'static str,
    /// The column with the anonymous id, if the table has one.
    pub anon_column: Option<&'static str>,
    /// The column with the user id, if the table has one.
    pub user_column: Option<&'static str>,
}

/// Every table that holds an anonymous id, a user id or a session id. All of
/// them have a `site_id` column.
pub const IDENTIFIER_TABLES: &[IdentifierTable] = &[
    IdentifierTable {
        name: "events",
        anon_column: Some("anon_id"),
        user_column: None,
    },
    IdentifierTable {
        name: "identity_alias",
        anon_column: Some("anon_id"),
        user_column: Some("user_id"),
    },
    IdentifierTable {
        name: "user_traits",
        anon_column: None,
        user_column: Some("user_id"),
    },
];

/// Anonymous ids per statement, so a statement stays far below ClickHouse's
/// query size limit (an id is at most 64 characters).
const ANON_IDS_PER_STATEMENT: usize = 500;
/// The most linked anonymous ids read for one user.
const MAX_LINKED_ANON_IDS: usize = 1_000_000;

/// Rows of one table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableCount {
    pub table: &'static str,
    pub rows: u64,
}

/// An identifier is put into a statement as a string literal, so it must be
/// of the shape the ingest path accepts: letters, digits and `_ . : -`, 1 to
/// 64 bytes. Anything else is refused, never escaped.
fn checked(id: &str) -> Result<&str, Error> {
    let fine = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b':' | b'-'));
    if fine {
        Ok(id)
    } else {
        Err(Error::InvalidName(
            "an identifier is 1 to 64 letters, digits and `_ . : -`".to_owned(),
        ))
    }
}

/// The conditions that select a user's rows in `table`, as statements apart:
/// first the rows of the user id, then the rows of the anonymous ids in
/// chunks, each excluding what an earlier one selects, so counting them adds
/// up without counting a row twice.
fn conditions(
    table: &IdentifierTable,
    site_id: u32,
    user_id: &str,
    anon_ids: &[String],
) -> Result<Vec<String>, Error> {
    let user = match table.user_column {
        Some(column) => Some(format!("{column} = '{}'", checked(user_id)?)),
        None => None,
    };
    let mut out = Vec::new();
    if let Some(user) = &user {
        out.push(format!("site_id = {site_id} AND {user}"));
    }
    if let Some(column) = table.anon_column {
        for chunk in anon_ids.chunks(ANON_IDS_PER_STATEMENT) {
            let list = chunk
                .iter()
                .map(|id| checked(id).map(|id| format!("'{id}'")))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ");
            let anons = format!("{column} IN ({list})");
            out.push(match &user {
                Some(user) => format!("site_id = {site_id} AND {anons} AND NOT ({user})"),
                None => format!("site_id = {site_id} AND {anons}"),
            });
        }
    }
    Ok(out)
}

impl Store {
    /// Runs one statement with extra ClickHouse settings in the URL.
    fn execute_with(&self, sql: &str, settings: &str) -> Result<String, Error> {
        let path = format!(
            "/?database={}&{settings}",
            http::url_encode(&self.config.database)
        );
        self.post(&path, sql.as_bytes(), false)
    }

    /// Every anonymous id ever linked to the user of the site, from the links
    /// table. A device that was linked to two users appears for each of them.
    pub fn linked_anon_ids(&self, site_id: u32, user_id: &str) -> Result<Vec<String>, Error> {
        let user = checked(user_id)?;
        let answer = self.execute(&format!(
            "SELECT DISTINCT anon_id FROM identity_alias FINAL
             WHERE site_id = {site_id} AND user_id = '{user}'
             ORDER BY anon_id LIMIT {MAX_LINKED_ANON_IDS} FORMAT TabSeparated"
        ))?;
        Ok(answer.lines().map(str::to_owned).collect())
    }

    /// How many rows of the user, and of the given anonymous ids, each
    /// identifier table holds. With `marked` the rows a lightweight delete
    /// has marked still count (what is still on disk); without it they do not
    /// (what a query sees).
    pub fn count_user_rows(
        &self,
        site_id: u32,
        user_id: &str,
        anon_ids: &[String],
        marked: bool,
    ) -> Result<Vec<TableCount>, Error> {
        let mut counts = Vec::with_capacity(IDENTIFIER_TABLES.len());
        for table in IDENTIFIER_TABLES {
            let mut rows = 0;
            for condition in conditions(table, site_id, user_id, anon_ids)? {
                let setting = if marked {
                    " SETTINGS apply_deleted_mask = 0"
                } else {
                    ""
                };
                let answer = self.execute(&format!(
                    "SELECT count() FROM {} WHERE {condition}{setting}",
                    table.name
                ))?;
                rows += answer
                    .trim()
                    .parse::<u64>()
                    .map_err(|_| Error::InvalidRow("a count that is not a number".to_owned()))?;
            }
            counts.push(TableCount {
                table: table.name,
                rows,
            });
        }
        Ok(counts)
    }

    /// Deletes the user's rows and those of the anonymous ids with a
    /// lightweight `DELETE`, and returns when ClickHouse has marked them
    /// (queries no longer see them). The data is still on disk until
    /// [`remove_deleted_rows`](Self::remove_deleted_rows).
    pub fn delete_user_rows(
        &self,
        site_id: u32,
        user_id: &str,
        anon_ids: &[String],
    ) -> Result<(), Error> {
        for table in IDENTIFIER_TABLES {
            for condition in conditions(table, site_id, user_id, anon_ids)? {
                self.execute_with(
                    &format!("DELETE FROM {} WHERE {condition}", table.name),
                    "lightweight_deletes_sync=2",
                )?;
            }
        }
        Ok(())
    }

    /// Rewrites the parts of every identifier table without the rows marked
    /// deleted, so that the data is gone from disk, and returns when the
    /// rewrite is done. It reads and writes the touched parts: run it once a
    /// day, not per request.
    pub fn remove_deleted_rows(&self) -> Result<(), Error> {
        for table in IDENTIFIER_TABLES {
            self.execute_with(
                &format!("ALTER TABLE {} APPLY DELETED MASK", table.name),
                "mutations_sync=2",
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrations::MIGRATIONS;

    #[test]
    fn every_table_with_an_identifier_column_is_registered() {
        let mut found = Vec::new();
        for migration in MIGRATIONS {
            for statement in migration.statements {
                let Some(rest) = statement.strip_prefix("CREATE TABLE IF NOT EXISTS ") else {
                    continue;
                };
                let name = rest.split_whitespace().next().unwrap_or("");
                let columns = statement.split_once('(').map_or("", |(_, body)| body);
                if ["anon_id", "user_id", "session_id"]
                    .iter()
                    .any(|column| columns.contains(column))
                {
                    found.push(name.to_owned());
                }
            }
        }
        assert!(!found.is_empty());
        for name in &found {
            assert!(
                IDENTIFIER_TABLES.iter().any(|table| table.name == name),
                "`{name}` holds an identifier and is not in IDENTIFIER_TABLES"
            );
        }
        for table in IDENTIFIER_TABLES {
            assert!(
                found.iter().any(|name| name == table.name),
                "`{}` is registered but holds no identifier",
                table.name
            );
        }
    }

    #[test]
    fn rollups_and_the_event_id_table_hold_no_identifier() {
        for name in ["drops_daily", "event_ids", "_migrations"] {
            assert!(IDENTIFIER_TABLES.iter().all(|table| table.name != name));
        }
    }

    #[test]
    fn the_user_rows_of_each_table_are_selected_once() {
        let anons: Vec<String> = (0..1200).map(|n| format!("a{n}")).collect();
        let [events, alias, traits] = [0, 1, 2].map(|i| &IDENTIFIER_TABLES[i]);

        let conditions_of = |table, anons: &[String]| conditions(table, 7, "u1", anons).unwrap();
        assert_eq!(conditions_of(events, &[]), Vec::<String>::new());
        let chunks = conditions_of(events, &anons);
        assert_eq!(chunks.len(), 3, "500 ids per statement");
        assert!(chunks[0].starts_with("site_id = 7 AND anon_id IN ('a0', 'a1'"));
        assert!(!chunks[0].contains("u1"), "events carry no user id");

        assert_eq!(
            conditions_of(traits, &anons),
            vec!["site_id = 7 AND user_id = 'u1'"],
            "traits are selected by user only"
        );
        let both = conditions_of(alias, &anons);
        assert_eq!(both.len(), 4);
        assert_eq!(both[0], "site_id = 7 AND user_id = 'u1'");
        assert!(
            both[1..]
                .iter()
                .all(|c| c.ends_with("AND NOT (user_id = 'u1')")),
            "a row selected by the user is not selected again by a device"
        );
        assert_eq!(
            conditions_of(alias, &[]),
            vec!["site_id = 7 AND user_id = 'u1'"]
        );
    }

    #[test]
    fn an_identifier_that_is_not_plain_is_refused_not_escaped() {
        let table = &IDENTIFIER_TABLES[1];
        for bad in [
            "",
            "a b",
            "a'b",
            "a\"b",
            "a;b",
            "a\\b",
            "a\nb",
            &"x".repeat(65),
        ] {
            assert!(conditions(table, 1, bad, &[]).is_err(), "user {bad:?}");
            assert!(
                conditions(table, 1, "u", &[bad.to_owned()]).is_err(),
                "device {bad:?}"
            );
        }
        // A table with no user column does not look at the user id.
        assert!(conditions(&IDENTIFIER_TABLES[0], 1, "x y", &["a".to_owned()]).is_ok());
    }
}
