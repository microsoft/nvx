use super::*;

fn message(value: &str) -> String {
    SandboxLayer::parse(value).unwrap_err().to_string()
}

#[test]
fn layers_require_canonical_uuids() {
    let layer =
        SandboxLayer::parse("distro,layers//distro.erofs,22222222-2222-2222-2222-22222222222a")
            .unwrap();
    assert_eq!(layer.role(), LayerRole::Distro);
    assert_eq!(layer.path(), Path::new("layers").join("distro.erofs"));
    assert_eq!(layer.uuid(), "22222222-2222-2222-2222-22222222222a");
    for uuid in [
        "{22222222-2222-2222-2222-22222222222a}",
        "urn:uuid:22222222-2222-2222-2222-22222222222a",
        "2222222222222222222222222222222a",
        "22222222-2222-2222-2222-22222222222A",
    ] {
        assert!(SandboxLayer::parse(&format!("distro,layer.erofs,{uuid}")).is_err());
    }
}

#[test]
fn layers_keep_the_format_of_their_image() {
    let erofs = SandboxLayer::new(
        LayerRole::Distro,
        "l//d.erofs",
        "22222222-2222-2222-2222-222222222222",
    );
    assert_eq!(erofs.format(), DistroFormat::Erofs);
    let gpt = SandboxLayer::gpt(
        LayerRole::Distro,
        "l//d.img",
        "22222222-2222-2222-2222-222222222222",
    );
    assert_eq!(gpt.format(), DistroFormat::Gpt);
    assert_eq!(gpt.path(), Path::new("l").join("d.img"));
    assert_eq!(gpt.uuid(), erofs.uuid());
    assert_eq!(gpt.role(), LayerRole::Distro);
    // `--layer` names EROFS layers.
    assert_eq!(
        SandboxLayer::parse("distro,l/d.erofs,22222222-2222-2222-2222-222222222222")
            .unwrap()
            .format(),
        DistroFormat::Erofs
    );
}

#[test]
fn layer_parser_rejects_invalid_role_and_uuid() {
    assert_eq!(LayerRole::from_name("distro"), Some(LayerRole::Distro));
    for role in ["unknown", "runtime", "custom", "Distro", ""] {
        assert_eq!(
            message(&format!(
                "{role},layer.erofs,22222222-2222-2222-2222-222222222222"
            )),
            format!("unsupported layer role {role:?}; choose distro")
        );
        assert_eq!(LayerRole::from_name(role), None, "{role}");
    }
    assert_eq!(
        message("distro,layer.erofs,not-a-uuid"),
        "distro layer UUID is invalid: \"not-a-uuid\""
    );
    assert_eq!(message("distro,,1"), "distro layer path is empty");
    for value in ["distro", "distro,a", "distro,a,b,c"] {
        assert_eq!(message(value), "--layer must be ROLE,PATH,EROFS_UUID");
    }
}
