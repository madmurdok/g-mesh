package server

// Logged wraps a Server by embedding it, which promotes every one of
// Server's methods into Logged's own method set without a line of syntax
// saying so. That is the third receiver shape the Go plugin has to resolve
// (after "through a variable" and "through an interface value"), and the one
// a structural tier cannot even see the possibility of: an embedded field is
// walked as an ordinary type reference and nothing more.
type Logged struct {
	*Server
	prefix string
}

// UseLogged calls a method Logged never declares. `l.Addr()` resolves to
// `Server.Addr` - the declaration, in a different file of this package -
// through the embedded field.
func UseLogged(l *Logged) string {
	return l.prefix + l.Addr()
}
