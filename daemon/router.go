package daemon

import (
	"net/http"

	"github.com/RunanywhereAI/wally/runanywhere"
)

type Router struct {
	sess   runanywhere.Session
	client *http.Client
}

func NewRouter(sess runanywhere.Session) *Router {
	return &Router{sess: sess, client: &http.Client{}}
}

func (rt *Router) endpoint(model string) (runanywhere.Endpoint, error) {
	return runanywhere.Resolve(model, rt.sess)
}
