// Package server is a tiny stand-in for a real Go server package, used only
// by plugins/go/conformance/project (GM-279's fixture) - real symbols and
// cross-file edges are GM-280's concern; this scaffold's extractor never
// looks inside this file, only at its existence and byte length.
package server

type Server struct {
	addr string
}

func New() *Server {
	return &Server{addr: ":8080"}
}

func (s *Server) Addr() string {
	return s.addr
}

func (s *Server) Close() error {
	return nil
}
