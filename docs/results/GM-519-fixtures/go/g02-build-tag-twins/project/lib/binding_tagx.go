//go:build tagx

package lib

type Binder interface {
	Bind() string
}

type impl struct{}

func (impl) Bind() string { return "msgpack" }

func New() Binder { return impl{} }
