#[cfg(feature = "hazmat-raw-access")]
#[test]
fn typestate_exclusion_compile_fail() {
    let t = trybuild::TestCases::new();
    t.compile_fail("tests/compile_fail/auto_spill_then_hazmat.rs");
    t.compile_fail("tests/compile_fail/hazmat_then_auto_spill.rs");
    t.compile_fail("tests/compile_fail/buddy_auto_spill_then_hazmat.rs");
    t.compile_fail("tests/compile_fail/buddy_hazmat_then_auto_spill.rs");
}
