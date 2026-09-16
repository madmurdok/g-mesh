package server

// Conn is a connection a Server hands out. Its field type `*Server` is
// declared in a sibling file of this same package, so the reference to it is
// a container-scoped placeholder rather than a direct edge.
type Conn struct {
	server *Server
}

// Closer is a project-local interface. Its method is a *declaration* -
// `Closer.Close` - and not a claim about any implementation.
type Closer interface {
	// Close releases whatever the implementation holds.
	Close() error
}

// helper is unexported and named exactly like the root package's own
// `helper` (helper.go). Neither can reach the other: they are in different
// containers, and each is visible only inside its own.
func helper() string {
	return "server"
}

func newConn(s *Server) *Conn {
	return &Conn{server: s}
}
