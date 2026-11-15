package main

import (
	"os"
	"regexp"
	"testing"
)

var rustStrConst = regexp.MustCompile(`(?m)^pub const ([A-Z_]+): &str = "([^"]*)";`)

// rustStrConsts reads every `pub const NAME: &str = "..."` out of a
// checked-in Rust source file as text, the way baseline_test.go reads
// BASELINE_EXCLUDED_DIRS.
func rustStrConsts(t *testing.T, path string) map[string]string {
	t.Helper()
	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v", path, err)
	}
	consts := map[string]string{}
	for _, match := range rustStrConst.FindAllSubmatch(content, -1) {
		consts[string(match[1])] = string(match[2])
	}
	return consts
}

// The Go plugin's native-kind strings are core's wire contract, defined in
// wire/src/native_kind.rs.
func TestNativeKindsEqualTheWireConstants(t *testing.T) {
	const path = "../../wire/src/native_kind.rs"
	wire := rustStrConsts(t, path)
	for _, pin := range []struct{ rust, goName, goValue string }{
		{"PENDING_SYMBOL_NATIVE_KIND", "pendingSymbolNativeKind", pendingSymbolNativeKind},
		{"RESOLVED_MODULE_NATIVE_KIND", "resolvedModuleNativeKind", resolvedModuleNativeKind},
		{"EXTERNAL_MODULE_NATIVE_KIND", "externalModuleNativeKind", externalModuleNativeKind},
		{"REEXPORT_ALL_NAME", "reexportAllName", reexportAllName},
	} {
		want, ok := wire[pin.rust]
		if !ok {
			t.Fatalf("%s declares no %s", path, pin.rust)
		}
		if pin.goValue != want {
			t.Errorf("%s %q must equal %s's %s %q", pin.goName, pin.goValue, path, pin.rust, want)
		}
	}
}
