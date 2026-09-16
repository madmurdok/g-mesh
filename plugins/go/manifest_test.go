package main

// The manifest and this binary have to agree about three things, and core
// checks exactly one of them (`protocol_version`, in
// `protocol::handshake::verify`). These tests are what check the other two,
// on the side that can: a Go test reading the checked-in plugin.toml.
//
// Read as text rather than through a TOML parser, because plugins/go/go.mod
// deliberately carries only the one dependency its semantic tier needs
// (golang.org/x/tools) and a whole TOML library to read three keys out of a
// file this small would not earn its place.

import (
	"os"
	"strings"
	"testing"
)

func manifestValue(t *testing.T, key string) string {
	t.Helper()
	content, err := os.ReadFile("plugin.toml")
	if err != nil {
		t.Fatalf("read plugin.toml: %v", err)
	}
	for _, raw := range strings.Split(string(content), "\n") {
		line := strings.TrimSpace(raw)
		if strings.HasPrefix(line, "#") {
			continue
		}
		name, value, found := strings.Cut(line, "=")
		if !found || strings.TrimSpace(name) != key {
			continue
		}
		return strings.Trim(strings.TrimSpace(value), `"`)
	}
	t.Fatalf("plugin.toml declares no %q", key)
	return ""
}

// The handshake announces a version core reads and `g-mesh plugins list`
// prints. A manifest saying one thing while the running binary says another
// is a discrepancy nobody would notice until they were debugging something
// else entirely.
func TestPluginVersionMatchesTheManifest(t *testing.T) {
	if got := manifestValue(t, "plugin_version"); got != pluginVersion {
		t.Fatalf("plugin.toml declares plugin_version %q, the binary announces %q", got, pluginVersion)
	}
}

func TestProtocolVersionAndLanguageMatchTheManifest(t *testing.T) {
	if got := manifestValue(t, "protocol_version"); got != "2" {
		t.Fatalf("plugin.toml declares protocol_version %q, the binary speaks %d", got, protocolVersion)
	}
	if got := manifestValue(t, "language"); got != languageName {
		t.Fatalf("plugin.toml declares language %q, the binary announces %q", got, languageName)
	}
}

// GM-281's capability flip, asserted from the plugin's own side as well as
// from core's (`core/tests/plugin_check.rs`'s
// the_go_manifest_declares_a_semantic_tier_that_resolves_receiver_calls):
// the semantic tier resolves receiver calls, the structural one still does
// not, and the difference is what keeps core's generated MCP instructions
// honest between the bulk walk and the first completed pass.
func TestManifestDeclaresTheSemanticTier(t *testing.T) {
	for key, want := range map[string]string{
		"semantic_pass":             "true",
		"receiver_calls":            "resolved",
		"receiver_calls_structural": "unresolved",
	} {
		if got := manifestValue(t, key); got != want {
			t.Fatalf("plugin.toml declares %s = %q, want %q", key, got, want)
		}
	}
}
