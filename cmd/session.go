package cmd

import (
	"context"
	"time"

	"github.com/RunanywhereAI/wally/console"
	"github.com/RunanywhereAI/wally/credstore"
	"github.com/RunanywhereAI/wally/runanywhere"
)

type session struct {
	store  *credstore.Store
	client *console.Client
}

func newSession() (*session, error) {
	store, err := credstore.New()
	if err != nil {
		return nil, err
	}
	return &session{store: store, client: console.New()}, nil
}

func (s *session) ConsoleBaseURL() string {
	return console.ResolveBaseURL()
}

func (s *session) signedIn() bool {
	creds, err := s.store.Load()
	return err == nil && creds.SignedIn()
}

func (s *session) Token() (string, error) {
	creds, err := s.store.Load()
	if err != nil {
		return "", err
	}
	if !creds.SignedIn() {
		return "", runanywhere.ErrNotSignedIn
	}
	if !creds.AccessTokenExpired(time.Now(), credstore.DefaultExpirySkew) {
		return creds.AccessToken, nil
	}

	grant, err := s.client.Refresh(context.Background(), creds.RefreshToken)
	if err != nil {
		return "", err
	}
	creds.AccessToken = grant.AccessToken
	creds.RefreshToken = grant.RefreshToken
	creds.Email = grant.Email
	creds.ExpiresAt = expiryUnix(grant.ExpiresIn)
	if err := s.store.Save(creds); err != nil {
		return "", err
	}
	return creds.AccessToken, nil
}
