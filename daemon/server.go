package daemon

import (
	"context"
	"net"
	"net/http"
	"os"

	"github.com/RunanywhereAI/wally/runanywhere"
)

const defaultAddr = "127.0.0.1:11500"

// Addr is the daemon's bind and dial address. WALLY_HOST overrides it as host:port.
func Addr() string {
	if h := os.Getenv("WALLY_HOST"); h != "" {
		return h
	}
	return defaultAddr
}

// LocalBaseURL is the OpenAI-compatible root first-party clients and harnesses point at.
func LocalBaseURL() string {
	return "http://" + Addr() + "/v1"
}

type Server struct {
	http   *http.Server
	router *Router
}

func New(sess runanywhere.Session) *Server {
	r := NewRouter(sess)
	mux := http.NewServeMux()
	r.register(mux)
	return &Server{
		http:   &http.Server{Addr: Addr(), Handler: mux},
		router: r,
	}
}

func (s *Server) ListenAndServe() error {
	ln, err := net.Listen("tcp", s.http.Addr)
	if err != nil {
		return err
	}
	return s.http.Serve(ln)
}

func (s *Server) Shutdown(ctx context.Context) error {
	return s.http.Shutdown(ctx)
}
