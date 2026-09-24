package main

// A test-only knob that parks the plugin at a named point, neither reading
// stdin nor writing stdout, for as long as a test wants it parked (GM-397).
// The contract is identical in all four bundled plugins - see
// plugins/sdk/src/hold.rs for why it exists:
//
// With G_MESH_PLUGIN_HOLD_DIR set to a directory, a plugin reaching hold
// point P for language L looks for <dir>/P-L.hold. If it exists, the plugin
// writes its own pid to <dir>/P-L.pid (through a temporary file and a
// rename) and then waits while the hold file exists, checking every 10ms,
// for at most 60s. The wait blocks the calling goroutine, as a real
// computation would. Unset, or with no hold file, it costs one stat at most.

import (
	"os"
	"path/filepath"
	"strconv"
	"time"
)

const (
	holdDirEnv   = "G_MESH_PLUGIN_HOLD_DIR"
	holdPoll     = 10 * time.Millisecond
	holdMax      = 60 * time.Second
	holdLanguage = "go"
)

// holdPoint parks the calling goroutine at point ("bulk" or "semantic") if
// the knob asks for it.
func holdPoint(point string) {
	dir := os.Getenv(holdDirEnv)
	if dir == "" {
		return
	}
	stem := point + "-" + holdLanguage
	hold := filepath.Join(dir, stem+".hold")
	if _, err := os.Stat(hold); err != nil {
		return
	}
	tmp := filepath.Join(dir, stem+".pid.tmp")
	if err := os.WriteFile(tmp, []byte(strconv.Itoa(os.Getpid())), 0o644); err != nil {
		logf("%s: failed to record the pid at hold point %s: %v", holdDirEnv, point, err)
	} else if err := os.Rename(tmp, filepath.Join(dir, stem+".pid")); err != nil {
		logf("%s: failed to record the pid at hold point %s: %v", holdDirEnv, point, err)
	}
	logf("%s: holding at %s while %s exists", holdDirEnv, point, hold)
	deadline := time.Now().Add(holdMax)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(hold); err != nil {
			return
		}
		time.Sleep(holdPoll)
	}
}
