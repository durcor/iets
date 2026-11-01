//! Complete store-path metadata shared by local discovery and both transports.

use std::io::{self, Read, Write};

use crate::error::{Error, Result};
use crate::store::path::StorePath;

use super::worker::wire::{
    self, read_bool, read_opt_path, read_paths, read_str, read_strs, read_time, read_u64,
    write_str, write_strs, write_u64,
};

/// One path's `UnkeyedValidPathInfo` at protocol 1.16, as it arrives in a
/// `PathInfo` message (path-info-wire.hh:`encodePathInfo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathInfo {
    pub path: StorePath,
    pub deriver: String,
    /// Base16, as the wire carries it.
    pub nar_hash: String,
    pub references: Vec<String>,
    pub registration_time: u64,
    pub nar_size: u64,
    pub ultimate: bool,
    pub sigs: Vec<String>,
    pub ca: String,
}

impl PathInfo {
    /// `nix path-info --json`'s full metadata, validated just like a wire reply.
    /// Older Nix versions omit empty optional fields.
    pub fn from_json(path: StorePath, value: &serde_json::Value) -> Result<PathInfo> {
        let bad = |message: &str| Error::Protocol(format!("nix path-info for '{path}': {message}"));
        let object = value.as_object().ok_or_else(|| bad("not an object"))?;
        let string = |key: &str| {
            object.get(key).and_then(|v| v.as_str())
                .ok_or_else(|| bad(&format!("'{key}' is not a string")))
        };
        let strings = |key: &str| -> Result<Vec<String>> {
            object.get(key).and_then(|v| v.as_array())
                .ok_or_else(|| bad(&format!("'{key}' is not a list")))?
                .iter()
                .map(|v| v.as_str().map(str::to_string)
                    .ok_or_else(|| bad(&format!("'{key}' contains a non-string"))))
                .collect()
        };
        let nar_hash = wire::nar_hash(string("narHash")?)?;
        let nar_size = object.get("narSize").and_then(|v| v.as_u64())
            .ok_or_else(|| bad("'narSize' is not an unsigned integer"))?;
        let references = strings("references")?;
        for reference in &references {
            StorePath::parse(reference)?;
        }
        let ca = match object.get("ca") {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(_) => wire::content_address(string("ca")?)?,
        };
        let deriver = match object.get("deriver") {
            None | Some(serde_json::Value::Null) => String::new(),
            Some(_) => {
                let deriver = string("deriver")?;
                if !deriver.is_empty() {
                    StorePath::parse(deriver)?;
                }
                deriver.to_string()
            }
        };
        let registration_time = match object.get("registrationTime") {
            None | Some(serde_json::Value::Null) => 0,
            Some(value) => value.as_u64().filter(|n| *n <= i64::MAX as u64)
                .ok_or_else(|| bad("'registrationTime' is not a valid time"))?,
        };
        let sigs = if object.contains_key("signatures") {
            strings("signatures")?.iter().map(|s| wire::signature(s))
                .collect::<io::Result<_>>()?
        } else {
            Vec::new()
        };
        let ultimate = match object.get("ultimate") {
            None => false,
            Some(value) => value.as_bool().ok_or_else(|| bad("'ultimate' is not a boolean"))?,
        };
        Ok(PathInfo {
            path, deriver, nar_hash, references, registration_time, nar_size, ultimate, sigs, ca,
        })
    }

    /// `entry.path` is a store path base name; `entry.info` the 1.16 bytes.
    pub fn decode(path: &str, info: &[u8]) -> Result<PathInfo> {
        let mut r = info;
        PathInfo::fields(StorePath::from_base_name(path)?, &mut r)
    }

    /// The same fields straight off a worker-protocol connection, where `path`
    /// is the full store path the caller asked `QueryPathInfo` about.
    pub fn read(path: &str, r: &mut dyn Read) -> Result<PathInfo> {
        PathInfo::fields(StorePath::parse(path)?, r)
    }

    fn fields(path: StorePath, r: &mut dyn Read) -> Result<PathInfo> {
        let deriver = read_opt_path(r)?;
        let nar_hash = wire::nar_hash(&read_str(r)?)?;
        let references = read_paths(r)?;
        let registration_time = read_time(r)?;
        let nar_size = read_u64(r)?;
        let ultimate = read_bool(r)?;
        let sigs = read_strs(r)?
            .iter()
            .map(|s| wire::signature(s))
            .collect::<io::Result<_>>()?;
        let ca = wire::content_address(&read_str(r)?)?;
        Ok(PathInfo {
            path,
            deriver,
            nar_hash,
            references,
            registration_time,
            nar_size,
            ultimate,
            sigs,
            ca,
        })
    }

    /// The `ValidPathInfo` an `AddMultipleToStore` body wants for this path
    /// (worker-protocol.cc:`Serialise<ValidPathInfo>` at 1.16): the path, then
    /// the same fields back, signatures included, so a local daemon can verify
    /// what the farm signed.
    pub fn write(&self, w: &mut dyn Write) -> io::Result<()> {
        write_str(w, self.path.to_string().as_bytes())?;
        write_str(w, self.deriver.as_bytes())?;
        write_str(w, self.nar_hash.as_bytes())?;
        write_strs(w, &self.references)?;
        write_u64(w, self.registration_time)?;
        write_u64(w, self.nar_size)?;
        write_u64(w, u64::from(self.ultimate))?;
        write_strs(w, &self.sigs)?;
        write_str(w, self.ca.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn path_info_round_trips() {
        let info = PathInfo {
            path: StorePath::from_base_name("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo").unwrap(),
            deriver: "/nix/store/ffffffffffffffffffffffffffffffff-foo.drv".into(),
            nar_hash: "0".repeat(64),
            references: vec!["/nix/store/cccccccccccccccccccccccccccccccc-src".into()],
            registration_time: 7,
            nar_size: 4096,
            ultimate: false,
            sigs: vec!["cache.numtide.com-1:abc=".into()],
            ca: String::new(),
        };
        // `write` prefixes the path, `decode` takes it out of band, so drop it.
        let mut bytes = Vec::new();
        info.write(&mut bytes).unwrap();
        let mut r = bytes.as_slice();
        assert_eq!(read_str(&mut r).unwrap(), info.path.to_string());
        let decoded = PathInfo::decode(info.path.base_name(), r).unwrap();
        assert_eq!(decoded, info);
    }

    #[test]
    fn malformed_local_metadata_is_not_an_unavailable_path() {
        let path = StorePath::from_base_name("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source").unwrap();
        let info = serde_json::json!({
            "narHash": format!("sha256:{}", "0".repeat(64)),
            "narSize": 128,
            "references": ["not-a-store-path"],
            "ca": null
        });
        assert!(PathInfo::from_json(path.clone(), &info).is_err());
        let mut info = info;
        info["references"] = serde_json::json!([]);
        info["ca"] = serde_json::json!("not-a-content-address");
        assert!(PathInfo::from_json(path.clone(), &info).is_err());
        info["ca"] = serde_json::Value::Null;
        info["signatures"] = serde_json::json!(["not-a-signature"]);
        assert!(PathInfo::from_json(path, &info).is_err());
    }

    #[test]
    fn local_metadata_keeps_ia_identity_and_signed_fingerprint() {
        let path = StorePath::from_base_name("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source").unwrap();
        let deriver = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-producer.drv";
        let info = serde_json::json!({
            "narHash": format!("sha256:{}", "0".repeat(64)),
            "narSize": 128,
            "references": [path.to_string()],
            "ca": null,
            "deriver": deriver,
            "registrationTime": 42,
            "signatures": ["cache:dGVzdA=="],
            "ultimate": true
        });
        let got = PathInfo::from_json(path.clone(), &info).unwrap();
        assert_eq!(got.path, path);
        assert_eq!(got.deriver, deriver);
        assert_eq!(got.references, [path.to_string()]);
        assert_eq!(got.nar_hash, "0".repeat(64));
        assert_eq!(got.nar_size, 128);
        assert_eq!(got.registration_time, 42);
        assert_eq!(got.sigs, ["cache:dGVzdA=="]);
        assert!(got.ca.is_empty());
        assert!(got.ultimate);
    }

}
