use prost_build::Config;
use std::path::Path;

#[derive(Debug)]
pub struct ProtoPaths<'a> {
    pub sns_wasm: &'a Path,
    pub sns_init: &'a Path,
    pub base_types: &'a Path,
    pub nervous_system: &'a Path,

    // Indirectly required by sns_init
    pub sns_swap: &'a Path,
}

/// Build protos using prost_build.
pub fn generate_prost_files(proto: ProtoPaths<'_>, out: &Path) {
    let proto_files = [proto.sns_wasm.join("ic_sns_wasm/pb/v1/sns_wasm.proto")];

    let mut config = Config::new();
    std::fs::create_dir_all(out).expect("failed to create output directory");
    config.out_dir(out);

    config.extern_path(".ic_base_types.pb.v1", "::ic-base-types");
    config.extern_path(
        ".ic_nervous_system.pb.v1",
        "::ic-nervous-system-proto::pb::v1",
    );
    config.extern_path(".ic_sns_init.pb.v1", "::ic-sns-init::pb::v1");
    config.extern_path(".ic_sns_swap.pb.v1", "::ic-sns-swap::pb::v1");

    // Add universally needed types to all definitions in this namespace
    config.type_attribute(
        ".ic_sns_wasm.pb.v1",
        "#[derive(candid::CandidType, candid::Deserialize, serde::Serialize)]",
    );
    // Add additional customizations
    ic_sns_type_attr(&mut config, "SnsVersion", "#[derive(Eq, Hash)]");
    ic_sns_type_attr(&mut config, "SnsCanisterIds", "#[derive(Copy)]");

    config.btree_map([".ic_sns_wasm.pb.v1.StableCanisterState.nns_proposal_to_deployed_sns"]);

    config
        .compile_protos(
            &proto_files,
            &[
                proto.base_types,
                proto.sns_init,
                proto.sns_wasm,
                proto.nervous_system,
                proto.sns_swap,
            ],
        )
        .unwrap();

    ic_utils_rustfmt::rustfmt(out).expect("failed to rustfmt protobufs");
}

/// Convenience function to add the correct namespace to our class names
fn ic_sns_type_attr<A>(cfg: &mut Config, class: &str, attributes: A)
where
    A: AsRef<str>,
{
    cfg.type_attribute("ic_sns_wasm.pb.v1.".to_owned() + class, attributes);
}
