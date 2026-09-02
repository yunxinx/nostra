use rusqlite::Connection;

use super::super::SessionDomain;
use super::{CATALOG_SCHEMA_VERSION, CatalogError};

#[derive(Clone, Copy)]
struct ColumnSchema {
    name: &'static str,
    declaration: &'static str,
    data_type: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
    primary_key_position: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ForeignKeySchema {
    table: &'static str,
    from: &'static str,
    to: &'static str,
    on_update: &'static str,
    on_delete: &'static str,
    match_name: &'static str,
}

#[derive(Clone, Copy)]
struct TableSchema {
    name: &'static str,
    columns: &'static [ColumnSchema],
    constraints: &'static [&'static str],
    foreign_keys: &'static [ForeignKeySchema],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IndexColumnSchema {
    name: &'static str,
    descending: bool,
}

#[derive(Clone, Copy)]
struct IndexSchema {
    name: &'static str,
    table: &'static str,
    columns: &'static [IndexColumnSchema],
    unique: bool,
    partial: bool,
}

const SESSIONS_COLUMNS: &[ColumnSchema] = &[
    ColumnSchema {
        name: "session_id",
        declaration: "session_id TEXT PRIMARY KEY NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    ColumnSchema {
        name: "domain",
        declaration: "domain TEXT NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "project_id",
        declaration: "project_id TEXT",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "canonical_path",
        declaration: "canonical_path TEXT",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "display_name",
        declaration: "display_name TEXT",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "title",
        declaration: "title TEXT",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "preview",
        declaration: "preview TEXT",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "model_profile_id",
        declaration: "model_profile_id TEXT",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "model_id",
        declaration: "model_id TEXT",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "total_tokens",
        declaration: "total_tokens INTEGER NOT NULL DEFAULT 0",
        data_type: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "created_at",
        declaration: "created_at INTEGER NOT NULL",
        data_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "updated_at",
        declaration: "updated_at INTEGER NOT NULL",
        data_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "favorited",
        declaration: "favorited INTEGER NOT NULL DEFAULT 0",
        data_type: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "jsonl_path",
        declaration: "jsonl_path TEXT NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const MESSAGE_NODE_COLUMNS: &[ColumnSchema] = &[
    ColumnSchema {
        name: "session_id",
        declaration: "session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE CASCADE",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    ColumnSchema {
        name: "entry_id",
        declaration: "entry_id TEXT NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 2,
    },
    ColumnSchema {
        name: "parent_id",
        declaration: "parent_id TEXT",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "timestamp",
        declaration: "timestamp INTEGER NOT NULL",
        data_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "role",
        declaration: "role TEXT NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "preview",
        declaration: "preview TEXT",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "searchable_text",
        declaration: "searchable_text TEXT NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "searchable_folded",
        declaration: "searchable_folded TEXT NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const MESSAGE_NODE_FOREIGN_KEYS: &[ForeignKeySchema] = &[ForeignKeySchema {
    table: "sessions",
    from: "session_id",
    to: "session_id",
    on_update: "NO ACTION",
    on_delete: "CASCADE",
    match_name: "NONE",
}];

const REPAIR_STATE_COLUMNS: &[ColumnSchema] = &[
    ColumnSchema {
        name: "key",
        declaration: "key TEXT PRIMARY KEY NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    ColumnSchema {
        name: "value",
        declaration: "value TEXT NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const PROJECT_COLUMNS: &[ColumnSchema] = &[
    ColumnSchema {
        name: "project_id",
        declaration: "project_id TEXT PRIMARY KEY NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 1,
    },
    ColumnSchema {
        name: "canonical_path",
        declaration: "canonical_path TEXT NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "display_name",
        declaration: "display_name TEXT NOT NULL",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ColumnSchema {
        name: "updated_at",
        declaration: "updated_at INTEGER NOT NULL",
        data_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
];

const SESSIONS_TABLE: TableSchema = TableSchema {
    name: "sessions",
    columns: SESSIONS_COLUMNS,
    constraints: &[],
    foreign_keys: &[],
};
const MESSAGE_NODES_TABLE: TableSchema = TableSchema {
    name: "message_nodes",
    columns: MESSAGE_NODE_COLUMNS,
    constraints: &["PRIMARY KEY(session_id, entry_id)"],
    foreign_keys: MESSAGE_NODE_FOREIGN_KEYS,
};
const REPAIR_STATE_TABLE: TableSchema = TableSchema {
    name: "repair_state",
    columns: REPAIR_STATE_COLUMNS,
    constraints: &[],
    foreign_keys: &[],
};
const PROJECTS_TABLE: TableSchema = TableSchema {
    name: "projects",
    columns: PROJECT_COLUMNS,
    constraints: &[],
    foreign_keys: &[],
};

const CHAT_TABLES: &[TableSchema] = &[SESSIONS_TABLE, MESSAGE_NODES_TABLE, REPAIR_STATE_TABLE];
const AGENT_TABLES: &[TableSchema] = &[
    SESSIONS_TABLE,
    MESSAGE_NODES_TABLE,
    REPAIR_STATE_TABLE,
    PROJECTS_TABLE,
];

const SESSION_DOMAIN_CREATED_COLUMNS: &[IndexColumnSchema] = &[
    IndexColumnSchema {
        name: "domain",
        descending: false,
    },
    IndexColumnSchema {
        name: "created_at",
        descending: true,
    },
    IndexColumnSchema {
        name: "session_id",
        descending: true,
    },
];
const SESSION_PROJECT_CREATED_COLUMNS: &[IndexColumnSchema] = &[
    IndexColumnSchema {
        name: "domain",
        descending: false,
    },
    IndexColumnSchema {
        name: "project_id",
        descending: false,
    },
    IndexColumnSchema {
        name: "created_at",
        descending: true,
    },
    IndexColumnSchema {
        name: "session_id",
        descending: true,
    },
];
const MESSAGE_TIMESTAMP_COLUMNS: &[IndexColumnSchema] = &[
    IndexColumnSchema {
        name: "session_id",
        descending: false,
    },
    IndexColumnSchema {
        name: "timestamp",
        descending: false,
    },
    IndexColumnSchema {
        name: "entry_id",
        descending: false,
    },
];
const MESSAGE_SEARCH_COLUMNS: &[IndexColumnSchema] = &[
    IndexColumnSchema {
        name: "session_id",
        descending: false,
    },
    IndexColumnSchema {
        name: "searchable_folded",
        descending: false,
    },
];

const CATALOG_INDEXES: &[IndexSchema] = &[
    IndexSchema {
        name: "sessions_domain_created",
        table: "sessions",
        columns: SESSION_DOMAIN_CREATED_COLUMNS,
        unique: false,
        partial: false,
    },
    IndexSchema {
        name: "sessions_project_created",
        table: "sessions",
        columns: SESSION_PROJECT_CREATED_COLUMNS,
        unique: false,
        partial: false,
    },
    IndexSchema {
        name: "message_nodes_session_timestamp",
        table: "message_nodes",
        columns: MESSAGE_TIMESTAMP_COLUMNS,
        unique: false,
        partial: false,
    },
    IndexSchema {
        name: "message_nodes_search",
        table: "message_nodes",
        columns: MESSAGE_SEARCH_COLUMNS,
        unique: false,
        partial: false,
    },
];

#[derive(Clone, Copy)]
pub(super) struct CatalogSchema {
    tables: &'static [TableSchema],
    indexes: &'static [IndexSchema],
}

impl CatalogSchema {
    pub(super) fn for_domain(domain: SessionDomain) -> Self {
        Self {
            tables: match domain {
                SessionDomain::Chat => CHAT_TABLES,
                SessionDomain::Agent => AGENT_TABLES,
            },
            indexes: CATALOG_INDEXES,
        }
    }

    pub(super) fn create(self, connection: &Connection) -> Result<(), CatalogError> {
        let mut sql = String::new();
        for table in self.tables {
            sql.push_str(&table_declaration(table));
            sql.push(';');
        }
        for index in self.indexes {
            sql.push_str("CREATE INDEX ");
            sql.push_str(index.name);
            sql.push_str(" ON ");
            sql.push_str(index.table);
            sql.push('(');
            for (column_index, column) in index.columns.iter().enumerate() {
                if column_index > 0 {
                    sql.push_str(", ");
                }
                sql.push_str(column.name);
                if column.descending {
                    sql.push_str(" DESC");
                }
            }
            sql.push_str(");");
        }
        sql.push_str(&format!("PRAGMA user_version = {CATALOG_SCHEMA_VERSION};"));
        connection.execute_batch(&sql)?;
        Ok(())
    }

    pub(super) fn validate(self, connection: &Connection) -> Result<(), CatalogError> {
        validate_executable_schema_objects(connection)?;
        validate_table_set(connection, self.tables)?;
        for table in self.tables {
            validate_table(connection, table)?;
        }
        validate_indexes(connection, self.indexes)?;
        Ok(())
    }
}

fn validate_executable_schema_objects(connection: &Connection) -> Result<(), CatalogError> {
    let mut statement = connection.prepare(
        "SELECT type, name FROM sqlite_schema
         WHERE type IN ('trigger', 'view')
         ORDER BY type, name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let unexpected = rows.collect::<Result<Vec<_>, _>>()?;
    if !unexpected.is_empty() {
        // Catalog triggers can rewrite or discard an otherwise valid
        // projection transaction while every table and index still matches.
        // Nostra owns the complete disposable schema, so any executable
        // object outside the manifest is a rebuildable shape mismatch.
        return Err(CatalogError::Corrupt(format!(
            "catalog contains unexpected executable schema objects: {unexpected:?}"
        )));
    }
    Ok(())
}

fn validate_table_set(
    connection: &Connection,
    expected: &[TableSchema],
) -> Result<(), CatalogError> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_schema
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    let actual = rows.collect::<Result<Vec<_>, _>>()?;
    let mut expected_names = expected
        .iter()
        .map(|table| table.name.to_string())
        .collect::<Vec<_>>();
    expected_names.sort();
    if actual != expected_names {
        return Err(CatalogError::Corrupt(format!(
            "catalog table set mismatch: expected {expected_names:?}, got {actual:?}"
        )));
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ActualColumn {
    name: String,
    data_type: String,
    not_null: bool,
    default_value: Option<String>,
    primary_key_position: i64,
}

fn validate_table(connection: &Connection, expected: &TableSchema) -> Result<(), CatalogError> {
    validate_table_declaration(connection, expected)?;
    let pragma = format!("PRAGMA table_info('{}')", expected.name);
    let mut statement = connection.prepare(&pragma)?;
    let rows = statement.query_map([], |row| {
        Ok(ActualColumn {
            name: row.get(1)?,
            data_type: row.get(2)?,
            not_null: row.get::<_, i64>(3)? != 0,
            default_value: row.get(4)?,
            primary_key_position: row.get(5)?,
        })
    })?;
    let actual = rows.collect::<Result<Vec<_>, _>>()?;
    if actual.len() != expected.columns.len() {
        return Err(CatalogError::Corrupt(format!(
            "catalog table `{}` has {} columns, expected {}",
            expected.name,
            actual.len(),
            expected.columns.len()
        )));
    }
    for (position, (actual, expected_column)) in actual.iter().zip(expected.columns).enumerate() {
        let matches = actual.name == expected_column.name
            && actual
                .data_type
                .eq_ignore_ascii_case(expected_column.data_type)
            && actual.not_null == expected_column.not_null
            && actual.default_value.as_deref() == expected_column.default_value
            && actual.primary_key_position == expected_column.primary_key_position;
        if !matches {
            return Err(CatalogError::Corrupt(format!(
                "catalog table `{}` column {position} mismatch: expected `{}` {} not_null={} default={:?} pk={}, got {actual:?}",
                expected.name,
                expected_column.name,
                expected_column.data_type,
                expected_column.not_null,
                expected_column.default_value,
                expected_column.primary_key_position,
            )));
        }
    }
    validate_foreign_keys(connection, expected)
}

fn validate_table_declaration(
    connection: &Connection,
    expected: &TableSchema,
) -> Result<(), CatalogError> {
    let actual = connection.query_row(
        "SELECT sql FROM sqlite_schema WHERE type = 'table' AND name = ?1",
        [expected.name],
        |row| row.get::<_, String>(0),
    )?;
    let expected_sql = table_declaration(expected);
    if actual.trim() != expected_sql {
        // PRAGMA table_info does not expose CHECK clauses, conflict policies,
        // collations, STRICT/WITHOUT ROWID modifiers, or extra UNIQUE
        // constraints. The catalog has no migration contract and is rebuilt
        // from JSONL, so accepting only the declaration generated by this
        // manifest is both safer and simpler than partially parsing SQL.
        return Err(CatalogError::Corrupt(format!(
            "catalog table `{}` declaration mismatch",
            expected.name
        )));
    }
    Ok(())
}

fn table_declaration(table: &TableSchema) -> String {
    let mut sql = String::from("CREATE TABLE ");
    sql.push_str(table.name);
    sql.push_str(" (");
    for (index, column) in table.columns.iter().enumerate() {
        if index > 0 {
            sql.push_str(", ");
        }
        sql.push_str(column.declaration);
    }
    for constraint in table.constraints {
        sql.push_str(", ");
        sql.push_str(constraint);
    }
    sql.push_str(") STRICT");
    sql
}

fn validate_foreign_keys(
    connection: &Connection,
    expected: &TableSchema,
) -> Result<(), CatalogError> {
    let pragma = format!("PRAGMA foreign_key_list('{}')", expected.name);
    let mut statement = connection.prepare(&pragma)?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, String>(7)?,
        ))
    })?;
    let actual = rows.collect::<Result<Vec<_>, _>>()?;
    if actual.len() != expected.foreign_keys.len() {
        return Err(CatalogError::Corrupt(format!(
            "catalog table `{}` has {} foreign keys, expected {}",
            expected.name,
            actual.len(),
            expected.foreign_keys.len()
        )));
    }
    for (actual, expected_key) in actual.iter().zip(expected.foreign_keys) {
        if actual.0 != expected_key.table
            || actual.1 != expected_key.from
            || actual.2 != expected_key.to
            || actual.3 != expected_key.on_update
            || actual.4 != expected_key.on_delete
            || actual.5 != expected_key.match_name
        {
            return Err(CatalogError::Corrupt(format!(
                "catalog table `{}` foreign-key shape mismatch",
                expected.name
            )));
        }
    }
    Ok(())
}

fn validate_indexes(connection: &Connection, expected: &[IndexSchema]) -> Result<(), CatalogError> {
    let mut statement = connection.prepare(
        "SELECT name, tbl_name FROM sqlite_schema
         WHERE type = 'index' AND sql IS NOT NULL
         ORDER BY name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let actual = rows.collect::<Result<Vec<_>, _>>()?;
    let mut expected_names = expected
        .iter()
        .map(|index| (index.name.to_string(), index.table.to_string()))
        .collect::<Vec<_>>();
    expected_names.sort();
    if actual != expected_names {
        return Err(CatalogError::Corrupt(format!(
            "catalog index set mismatch: expected {expected_names:?}, got {actual:?}"
        )));
    }

    for expected_index in expected {
        let index_list = format!("PRAGMA index_list('{}')", expected_index.table);
        let mut statement = connection.prepare(&index_list)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? != 0,
                row.get::<_, i64>(4)? != 0,
            ))
        })?;
        let (actual_unique, actual_partial) = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find_map(|(name, unique, partial)| {
                (name == expected_index.name).then_some((unique, partial))
            })
            .ok_or_else(|| {
                CatalogError::Corrupt(format!(
                    "catalog index `{}` is missing from table `{}`",
                    expected_index.name, expected_index.table
                ))
            })?;
        if actual_unique != expected_index.unique {
            return Err(CatalogError::Corrupt(format!(
                "catalog index `{}` uniqueness mismatch",
                expected_index.name
            )));
        }
        if actual_partial != expected_index.partial {
            return Err(CatalogError::Corrupt(format!(
                "catalog index `{}` partial-index mismatch",
                expected_index.name
            )));
        }

        let pragma = format!("PRAGMA index_xinfo('{}')", expected_index.name);
        let mut statement = connection.prepare(&pragma)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(5)? != 0,
            ))
        })?;
        let actual_columns = rows
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(name, descending, key)| key.then_some((name?, descending)))
            .collect::<Vec<_>>();
        if actual_columns.len() != expected_index.columns.len()
            || actual_columns.iter().zip(expected_index.columns).any(
                |((name, descending), expected_column)| {
                    name != expected_column.name || *descending != expected_column.descending
                },
            )
        {
            return Err(CatalogError::Corrupt(format!(
                "catalog index `{}` column shape mismatch",
                expected_index.name
            )));
        }
    }
    Ok(())
}
