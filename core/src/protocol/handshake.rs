use std::io::BufRead;

use anyhow::{bail, Context, Result};

use crate::protocol::jsonrpc::read_message;
use crate::protocol::types::{Handshake, CURRENT_PROTOCOL_VERSION};

/// A protocol is code, not data - unlike a schema_version mismatch (which
/// triggers a reindex), there is nothing to reconcile here, so a mismatch
/// is a hard load failure with a clear, actionable error.
pub fn verify(handshake: &Handshake) -> Result<()> {
    if handshake.protocol_version != CURRENT_PROTOCOL_VERSION {
        bail!(
            "protocol version mismatch with {} plugin (plugin version {}): core expects protocol version {}, plugin speaks {} - refusing to load; update the plugin to match",
            handshake.language,
            handshake.plugin_version,
            CURRENT_PROTOCOL_VERSION,
            handshake.protocol_version,
        );
    }
    Ok(())
}

/// Reads the plugin's handshake off its stdout and verifies it before any
/// further protocol exchange happens.
pub fn perform<R: BufRead>(reader: &mut R) -> Result<Handshake> {
    let handshake: Handshake = read_message(reader)
        .context("failed to read plugin handshake")?
        .context("plugin closed its stdout before sending a handshake")?;
    verify(&handshake)?;
    Ok(handshake)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::jsonrpc::write_message;
    use std::io::{BufReader, Cursor};

    fn handshake(protocol_version: u32) -> Handshake {
        Handshake {
            protocol_version,
            language: "typescript".to_string(),
            plugin_version: "0.1.0".to_string(),
        }
    }

    #[test]
    fn matching_version_verifies_ok() {
        assert!(verify(&handshake(CURRENT_PROTOCOL_VERSION)).is_ok());
    }

    #[test]
    fn mismatched_version_fails_with_both_versions_named() {
        let bad = CURRENT_PROTOCOL_VERSION + 1;
        let err = verify(&handshake(bad)).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(&CURRENT_PROTOCOL_VERSION.to_string()),
            "error must name the expected version: {message}"
        );
        assert!(message.contains(&bad.to_string()), "error must name the actual version: {message}");
        assert!(message.contains("typescript"));
    }

    #[test]
    fn perform_succeeds_on_matching_handshake_frame() {
        let mut buf = Vec::new();
        write_message(&mut buf, &handshake(CURRENT_PROTOCOL_VERSION)).unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let received = perform(&mut reader).unwrap();
        assert_eq!(received.protocol_version, CURRENT_PROTOCOL_VERSION);
    }

    #[test]
    fn perform_aborts_on_mismatched_handshake_frame() {
        let mut buf = Vec::new();
        write_message(&mut buf, &handshake(CURRENT_PROTOCOL_VERSION + 1)).unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        assert!(perform(&mut reader).is_err());
    }

    #[test]
    fn perform_fails_clearly_when_plugin_closes_before_handshaking() {
        let mut reader = BufReader::new(Cursor::new(Vec::new()));
        let err = perform(&mut reader).unwrap_err();
        assert!(err.to_string().contains("before sending a handshake"));
    }
}
