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

// Close releases the connection by closing the server behind it.
//
// Two shapes in one line, and neither is go/parser's answer: `c.server` is a
// field access through a value (this plugin emits no node for a struct
// field, so nothing is emitted for it at all), and `.Close()` is a method
// call through that field's type - `Server.Close`, declared in a sibling
// file, which only a type checker can name.
//
// Declaring it also makes Conn an *implicit* implementer of Closer. Nothing
// in this file says so; `types.Implements` is what says so.
func (c *Conn) Close() error {
	return c.server.Close()
}

// CloseAll closes through an interface value. The call site can only name
// `Closer.Close` - which concrete method runs is decided at run time - so
// that is exactly what the semantic pass attributes it to, and
// find_implementations is what gets a reader from there to Conn and Server.
func CloseAll(closers []Closer) error {
	for _, c := range closers {
		if err := c.Close(); err != nil {
			return err
		}
	}
	return nil
}
