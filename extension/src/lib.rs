use pgrx::prelude::*;

::pgrx::pg_module_magic!(name, version);

/// Phase 0 smoke-test entry point: `SELECT pgmind_version();`
#[pg_extern]
fn pgmind_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn version_is_reported_via_sql() {
        // owned String: a returned &str would borrow SPI memory freed before use
        let version = Spi::get_one::<String>("SELECT pgmind_version();")
            .expect("SPI call failed")
            .expect("pgmind_version() returned NULL");
        assert_eq!(version, env!("CARGO_PKG_VERSION"));
    }
}

/// This module is required by `cargo pgrx test` invocations.
/// It must be visible at the root of your extension crate.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec![]
    }
}
