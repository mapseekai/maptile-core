use std::collections::HashMap;

use super::{
    CompositeDeclaration, CompositeLimits, DuplicateLayer, MemberFailurePolicy, compile_graph,
};

#[test]
fn lazy_composite_with_missing_member_has_no_leaves() {
    let declarations = [CompositeDeclaration {
        id: "x".into(),
        members: vec!["missing".into()],
        lazy: true,
        duplicate_layer: DuplicateLayer::Error,
        failure_policy: MemberFailurePolicy::FailFast,
    }];
    let graph = compile_graph(&HashMap::new(), &declarations, CompositeLimits::default()).unwrap();
    assert!(graph["x"].leaves().is_empty());
}
