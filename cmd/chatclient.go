package cmd

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"os/signal"
	"strings"

	"github.com/RunanywhereAI/wally/daemon"
)

type chatMessage struct {
	Role    string `json:"role"`
	Content string `json:"content"`
}

// streamTurn runs one chat exchange with the current generation cancellable by
// Ctrl+C: the handler is installed only for the turn and removed after, so the
// REPL re-arms it each time and a cancel returns to the prompt rather than
// killing the process.
func streamTurn(model string, messages []chatMessage, out io.Writer) (string, error) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	sig := make(chan os.Signal, 1)
	signal.Notify(sig, os.Interrupt)
	defer signal.Stop(sig)
	go func() {
		<-sig
		cancel()
	}()
	return streamCompletion(ctx, model, messages, out)
}

// streamCompletion sends messages to the daemon chat route, streams the
// assistant text to out as it arrives, and returns the full assistant reply. An
// error frame that arrives after the 200 is surfaced rather than dropped.
func streamCompletion(ctx context.Context, model string, messages []chatMessage, out io.Writer) (string, error) {
	reqBody, err := json.Marshal(map[string]any{
		"model":    model,
		"messages": messages,
		"stream":   true,
	})
	if err != nil {
		return "", err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, daemon.LocalBaseURL()+"/chat/completions", bytes.NewReader(reqBody))
	if err != nil {
		return "", err
	}
	req.Header.Set("Content-Type", "application/json")

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		return "", responseError(resp)
	}

	var full strings.Builder
	reader := bufio.NewReader(resp.Body)
	for {
		line, readErr := reader.ReadString('\n')
		if trimmed := strings.TrimSpace(line); strings.HasPrefix(trimmed, "data:") {
			payload := strings.TrimSpace(strings.TrimPrefix(trimmed, "data:"))
			if payload == "[DONE]" {
				break
			}
			var chunk struct {
				Error *struct {
					Message string `json:"message"`
				} `json:"error"`
				Choices []struct {
					Delta struct {
						Content string `json:"content"`
					} `json:"delta"`
				} `json:"choices"`
			}
			if json.Unmarshal([]byte(payload), &chunk) == nil {
				if chunk.Error != nil && chunk.Error.Message != "" {
					io.WriteString(out, "\n")
					return full.String(), errors.New(chunk.Error.Message)
				}
				for _, ch := range chunk.Choices {
					if ch.Delta.Content != "" {
						io.WriteString(out, ch.Delta.Content)
						full.WriteString(ch.Delta.Content)
					}
				}
			}
		}
		if readErr != nil {
			break
		}
	}
	io.WriteString(out, "\n")
	return full.String(), nil
}

func responseError(resp *http.Response) error {
	body, _ := io.ReadAll(resp.Body)
	var parsed struct {
		Error struct {
			Message string `json:"message"`
		} `json:"error"`
	}
	if json.Unmarshal(body, &parsed) == nil && parsed.Error.Message != "" {
		return errors.New(parsed.Error.Message)
	}
	return fmt.Errorf("the model endpoint returned status %d", resp.StatusCode)
}
