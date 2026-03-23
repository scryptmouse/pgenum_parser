# pgenum_parser

This crate provides a parser for PostgreSQL enum types, intended to allow static analysis of enum definitions in SQL dumps (such as provided by Rails' `structure.sql`). It uses [pg_query.rs](https://github.com/pganalyze/pg_query.rs) for the analysis of the SQL and can output a structured representation of the enum types defined in the SQL for later consumption.

```rust
use pgenum_parser::{Database};

fn main() -> anyhow::Result<()> {
    let sql = std::fs::read_to_string("db/structure.sql")?;
    let enums = Database::from_sql("my_database", &sql)?;
    println!("{:#?}", enums);

    for enm in enums.enums {
        println!("Enum: {}.{}", enm.schema, enm.name);

        for value in &enm.values {
            println!("  - {}", value);
        }
    }

    Ok(())
}
```
