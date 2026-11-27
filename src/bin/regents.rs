// The `regents` binary is the herdr program built a second time. Only the
// product identity (`crate::build_info::product_name()`) differs, so the fork
// keeps internal names and stays cheap to sync with upstream (ADR-0002).
include!("../main.rs");
