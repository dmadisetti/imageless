//! Seed-image assembly: config and manifest JSON around the packed layer.
//!
//! The layer ships uncompressed (`application/vnd.oci.image.layer.v1.tar`),
//! so its diff_id and blob digest are the same value and every digest here is
//! computed over the exact bytes that would be pushed — serialized once, then
//! hashed, never re-encoded.

use serde::Serialize;
use sha2::{Digest, Sha256};

pub const LAYER_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar";
pub const CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
pub const MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

pub struct SeedImage {
    pub layer: Vec<u8>,
    pub config: Vec<u8>,
    pub manifest: Vec<u8>,
    pub layer_digest: String,
    pub config_digest: String,
    pub manifest_digest: String,
}

#[derive(Serialize)]
struct ImageConfig<'a> {
    architecture: &'a str,
    os: &'a str,
    config: EmptyProcessConfig,
    rootfs: RootFs<'a>,
}

/// Deliberately empty: the seed image carries no entrypoint or environment.
/// The pod manifest names the command, and the materialized rootfs is the
/// filesystem — nothing in the seed should influence execution.
#[derive(Serialize)]
struct EmptyProcessConfig {}

#[derive(Serialize)]
struct RootFs<'a> {
    #[serde(rename = "type")]
    kind: &'a str,
    diff_ids: Vec<String>,
}

#[derive(Serialize)]
struct Manifest<'a> {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(rename = "mediaType")]
    media_type: &'a str,
    config: Descriptor<'a>,
    layers: Vec<Descriptor<'a>>,
}

#[derive(Serialize)]
struct Descriptor<'a> {
    #[serde(rename = "mediaType")]
    media_type: &'a str,
    digest: String,
    size: u64,
}

pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

pub fn assemble(layer: Vec<u8>, architecture: &str) -> SeedImage {
    let layer_digest = digest(&layer);
    let config = serde_json::to_vec(&ImageConfig {
        architecture,
        os: "linux",
        config: EmptyProcessConfig {},
        rootfs: RootFs {
            kind: "layers",
            diff_ids: vec![layer_digest.clone()],
        },
    })
    .expect("static structure serializes");
    let config_digest = digest(&config);
    let manifest = serde_json::to_vec(&Manifest {
        schema_version: 2,
        media_type: MANIFEST_MEDIA_TYPE,
        config: Descriptor {
            media_type: CONFIG_MEDIA_TYPE,
            digest: config_digest.clone(),
            size: config.len() as u64,
        },
        layers: vec![Descriptor {
            media_type: LAYER_MEDIA_TYPE,
            digest: layer_digest.clone(),
            size: layer.len() as u64,
        }],
    })
    .expect("static structure serializes");
    let manifest_digest = digest(&manifest);
    SeedImage {
        layer,
        config,
        manifest,
        layer_digest,
        config_digest,
        manifest_digest,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uncompressed_layer_diff_id_equals_the_blob_digest() {
        let image = assemble(b"layer-bytes".to_vec(), "amd64");
        let config: serde_json::Value = serde_json::from_slice(&image.config).unwrap();
        assert_eq!(
            config["rootfs"]["diff_ids"][0].as_str().unwrap(),
            image.layer_digest
        );
        assert_eq!(config["architecture"], "amd64");
        assert_eq!(config["os"], "linux");
    }

    #[test]
    fn every_digest_covers_the_exact_serialized_bytes() {
        let image = assemble(b"layer-bytes".to_vec(), "amd64");
        assert_eq!(digest(&image.layer), image.layer_digest);
        assert_eq!(digest(&image.config), image.config_digest);
        assert_eq!(digest(&image.manifest), image.manifest_digest);
        let manifest: serde_json::Value = serde_json::from_slice(&image.manifest).unwrap();
        assert_eq!(manifest["schemaVersion"], 2);
        assert_eq!(
            manifest["config"]["digest"].as_str().unwrap(),
            image.config_digest
        );
        assert_eq!(
            manifest["config"]["size"].as_u64().unwrap(),
            image.config.len() as u64
        );
        assert_eq!(manifest["layers"][0]["mediaType"], LAYER_MEDIA_TYPE);
        assert_eq!(
            manifest["layers"][0]["size"].as_u64().unwrap(),
            image.layer.len() as u64
        );
    }

    #[test]
    fn assembly_is_deterministic() {
        let first = assemble(b"same".to_vec(), "amd64");
        let second = assemble(b"same".to_vec(), "amd64");
        assert_eq!(first.manifest, second.manifest);
        assert_eq!(first.manifest_digest, second.manifest_digest);
    }
}
