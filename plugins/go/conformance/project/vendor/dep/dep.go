// Never indexed: vendor/ is one of plugin.toml's exclude_dirs. Its presence
// in this fixture is the point - it exercises that exclusion.
package dep

func F() int { return 1 }
