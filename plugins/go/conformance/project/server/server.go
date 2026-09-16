// Package server is a small stand-in for a real Go server package.
package server

// Server is this package's one exported type.
type Server struct {
	addr string
	tag  string
}

// New builds a Server. Its call to `helper` is the fixture's second
// cross-file, same-package link - in a *different* package from main.go's,
// against an identically named target, which is what makes the two caller
// expectations in expect.toml discriminate between containers rather than
// just finding some symbol called `helper`.
func New() *Server {
	return &Server{addr: ":8080", tag: helper()}
}

// Addr reports the address this server listens on.
func (s *Server) Addr() string {
	return s.addr
}

// Close releases the server's resources. Declared with a pointer receiver,
// so its qualifiedName is `Server.Close` and the receiver kind lives in the
// signature. `Closer.Close` in conn.go declares the same name on an
// interface; nothing structural claims that Server satisfies Closer, because
// Go's interfaces are structural and only go/types can answer that (GM-281).
func (s *Server) Close() error {
	return nil
}
