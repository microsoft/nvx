use super::*;

#[test]
fn nearest_rank_uses_the_harness_definition() {
    let values = [1.0, 2.0, 3.0, 4.0];
    assert_eq!(nearest_rank(&values, 0.50), 2.0);
    assert_eq!(nearest_rank(&values, 0.99), 4.0);
}
