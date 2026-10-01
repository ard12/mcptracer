// Real Go SDK v1.7.0 cancellation client and server for MCPTracer's focused smoke.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/modelcontextprotocol/go-sdk/mcp"
)

func main() {
	if len(os.Args) > 1 && os.Args[1] == "server" {
		if err := runServer(os.Args[2:]); err != nil {
			fmt.Fprintln(os.Stderr, err)
			os.Exit(1)
		}
		return
	}
	if err := runSmoke(); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func runServer(args []string) error {
	if len(args) != 1 {
		return fmt.Errorf("server mode requires one cancellation marker path")
	}
	marker := args[0]
	server := mcp.NewServer(&mcp.Implementation{Name: "mcptracer-go-cancellation", Version: "1.0.0"}, nil)
	mcp.AddTool(server, &mcp.Tool{Name: "wait_for_cancel", Description: "Wait until the client cancels this call."},
		func(ctx context.Context, req *mcp.CallToolRequest, _ struct{}) (*mcp.CallToolResult, any, error) {
			token := req.Params.GetProgressToken()
			if token == nil {
				return nil, nil, fmt.Errorf("call did not include a progress token")
			}
			if err := req.Session.NotifyProgress(ctx, &mcp.ProgressNotificationParams{
				ProgressToken: token,
				Progress:      1,
				Total:         2,
				Message:       "handler-started",
			}); err != nil {
				return nil, nil, err
			}
			<-ctx.Done()
			if err := os.WriteFile(marker, []byte("cancelled"), 0o600); err != nil {
				return nil, nil, err
			}
			time.Sleep(25 * time.Millisecond)
			return nil, nil, ctx.Err()
		})
	err := server.Run(context.Background(), &mcp.StdioTransport{})
	if err != nil && strings.Contains(err.Error(), "server is closing: EOF") {
		return nil
	}
	return err
}

func runSmoke() error {
	binary := os.Getenv("MCPTRACER_BIN")
	if binary == "" {
		return fmt.Errorf("MCPTRACER_BIN must name the MCPTracer executable")
	}
	binary, err := filepath.Abs(binary)
	if err != nil {
		return err
	}
	if _, err := os.Stat(binary); err != nil {
		return fmt.Errorf("MCPTRACER_BIN is unavailable: %w", err)
	}
	self, err := os.Executable()
	if err != nil {
		return err
	}
	root, err := os.MkdirTemp("", "mcptracer-go-sdk-cancel-")
	if err != nil {
		return err
	}
	defer os.RemoveAll(root)
	db := filepath.Join(root, "sessions.db")
	sourceMarker := filepath.Join(root, "source-cancelled.txt")
	if err := captureAndCancel(binary, self, db, sourceMarker); err != nil {
		return fmt.Errorf("source cancellation: %w", err)
	}
	if err := waitForFile(sourceMarker, 3*time.Second); err != nil {
		return fmt.Errorf("real Go SDK cancellation did not reach server cleanup: %w", err)
	}
	sourceID, err := latestSessionID(binary, db)
	if err != nil {
		return err
	}
	if err := verifyCancelledSession(binary, db, sourceID); err != nil {
		return fmt.Errorf("source capture: %w", err)
	}

	replayMarker := filepath.Join(root, "replay-cancelled.txt")
	replay, err := runCLI(binary, db, 20*time.Second, "replay", sourceID,
		"--timing", "realtime", "--request-timeout", "3000", "--i-understand-side-effects",
		"--", self, "server", replayMarker)
	if err != nil {
		return fmt.Errorf("replay process: %w", err)
	}
	if replay.ExitCode != 0 {
		return fmt.Errorf("replay failed (%d): %s", replay.ExitCode, replay.Stderr)
	}
	replayID, err := sessionIDFromReplay(replay.Stderr)
	if err != nil {
		return err
	}
	if err := waitForFile(replayMarker, 3*time.Second); err != nil {
		return fmt.Errorf("replay did not cancel real Go SDK server handler: %w", err)
	}
	if err := verifyCancelledSession(binary, db, replayID); err != nil {
		return fmt.Errorf("replay capture: %w", err)
	}
	diff, err := runCLI(binary, db, 20*time.Second, "diff", sourceID, replayID, "--ignore-latency")
	if err != nil {
		return err
	}
	if diff.ExitCode != 0 {
		return fmt.Errorf("source/replay diff failed (%d): %s%s", diff.ExitCode, diff.Stdout, diff.Stderr)
	}
	fmt.Println("PASS: Go SDK v1.7.0 sent stdio cancellation; capture and replay validate cancelled; diff is clean")
	return nil
}

func captureAndCancel(binary, self, db, marker string) error {
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	progress := make(chan struct{}, 1)
	var stderr bytes.Buffer
	cmd := exec.Command(binary, "--db", db, "record", "--client", "go-v1-cancellation", "--", self, "server", marker)
	cmd.Stderr = &stderr
	client := mcp.NewClient(&mcp.Implementation{Name: "mcptracer-go-cancellation-client", Version: "1.0.0"}, &mcp.ClientOptions{
		ProgressNotificationHandler: func(context.Context, *mcp.ProgressNotificationClientRequest) {
			select {
			case progress <- struct{}{}:
			default:
			}
		},
	})
	session, err := client.Connect(ctx, &mcp.CommandTransport{Command: cmd}, nil)
	if err != nil {
		return fmt.Errorf("connect Go SDK client to MCPTracer record: %w; stderr: %s", err, stderr.String())
	}
	callCtx, cancelCall := context.WithCancel(ctx)
	callDone := make(chan error, 1)
	params := &mcp.CallToolParams{Name: "wait_for_cancel"}
	params.SetProgressToken("go-cancellation-smoke")
	go func() {
		_, callErr := session.CallTool(callCtx, params)
		callDone <- callErr
	}()
	select {
	case <-progress:
	case <-ctx.Done():
		cancelCall()
		_ = session.Close()
		return fmt.Errorf("timed out waiting for server progress: %w; stderr: %s", ctx.Err(), stderr.String())
	}
	time.Sleep(50 * time.Millisecond)
	cancelCall()
	select {
	case callErr := <-callDone:
		if !errors.Is(callErr, context.Canceled) {
			_ = session.Close()
			return fmt.Errorf("CallTool error = %v, want context.Canceled", callErr)
		}
	case <-ctx.Done():
		_ = session.Close()
		return fmt.Errorf("CallTool did not return after context cancellation: %w", ctx.Err())
	}
	if err := session.Ping(ctx, nil); err != nil {
		return fmt.Errorf("post-cancellation ping failed: %w", err)
	}
	if err := session.Close(); err != nil {
		return fmt.Errorf("close Go SDK session: %w; stderr: %s", err, stderr.String())
	}
	return nil
}

type commandResult struct {
	ExitCode int
	Stdout   string
	Stderr   string
}

func runCLI(binary, db string, timeout time.Duration, args ...string) (commandResult, error) {
	all := append([]string{"--db", db}, args...)
	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, binary, all...)
	var stdout, stderr bytes.Buffer
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	err := cmd.Run()
	result := commandResult{Stdout: stdout.String(), Stderr: stderr.String()}
	if exitErr, ok := err.(*exec.ExitError); ok {
		result.ExitCode = exitErr.ExitCode()
		return result, nil
	}
	if err != nil {
		return result, fmt.Errorf("run mcptracer %v: %w (stderr: %s)", args, err, stderr.String())
	}
	return result, nil
}

func latestSessionID(binary, db string) (string, error) {
	result, err := runCLI(binary, db, 10*time.Second, "sessions", "list", "--json")
	if err != nil {
		return "", err
	}
	if result.ExitCode != 0 {
		return "", fmt.Errorf("sessions list failed: %s", result.Stderr)
	}
	var sessions []struct {
		ID string `json:"id"`
	}
	if err := json.Unmarshal([]byte(result.Stdout), &sessions); err != nil {
		return "", fmt.Errorf("decode sessions list: %w", err)
	}
	if len(sessions) == 0 {
		return "", fmt.Errorf("sessions list returned no captures")
	}
	return sessions[len(sessions)-1].ID, nil
}

func verifyCancelledSession(binary, db, id string) error {
	validation, err := runCLI(binary, db, 10*time.Second, "validate", id, "--json")
	if err != nil {
		return err
	}
	if validation.ExitCode != 0 {
		return fmt.Errorf("validate failed: stderr=%s stdout=%s", validation.Stderr, validation.Stdout)
	}
	var health struct {
		Healthy bool `json:"healthy"`
	}
	if err := json.Unmarshal([]byte(validation.Stdout), &health); err != nil || !health.Healthy {
		return fmt.Errorf("validation was not healthy: output=%s err=%v", validation.Stdout, err)
	}
	calls, err := runCLI(binary, db, 10*time.Second, "sessions", "show", id, "--calls", "--json")
	if err != nil {
		return err
	}
	if calls.ExitCode != 0 {
		return fmt.Errorf("session calls failed: %s", calls.Stderr)
	}
	var model struct {
		Exchanges []struct {
			Method string `json:"method"`
			Status string `json:"status"`
		} `json:"exchanges"`
		Stats struct {
			Cancelled int `json:"cancelled"`
		} `json:"stats"`
	}
	if err := json.Unmarshal([]byte(calls.Stdout), &model); err != nil {
		return fmt.Errorf("decode correlated calls: %w", err)
	}
	found := false
	for _, exchange := range model.Exchanges {
		if exchange.Method == "tools/call" {
			found = exchange.Status == "cancelled"
			break
		}
	}
	if !found || model.Stats.Cancelled != 1 {
		return fmt.Errorf("expected one cancelled tools/call; exchanges=%+v stats=%+v", model.Exchanges, model.Stats)
	}
	raw, err := runCLI(binary, db, 10*time.Second, "sessions", "show", id, "--full", "--json")
	if err != nil {
		return err
	}
	if raw.ExitCode != 0 {
		return fmt.Errorf("session messages failed: %s", raw.Stderr)
	}
	var messages []struct {
		Method string `json:"method"`
	}
	if err := json.Unmarshal([]byte(raw.Stdout), &messages); err != nil {
		return fmt.Errorf("decode raw messages: %w", err)
	}
	for _, message := range messages {
		if message.Method == "notifications/cancelled" {
			return nil
		}
	}
	return fmt.Errorf("raw capture lacks notifications/cancelled")
}

func sessionIDFromReplay(stderr string) (string, error) {
	for _, line := range strings.Split(stderr, "\n") {
		const prefix = "replaying session "
		if at := strings.Index(line, prefix); at >= 0 {
			line = line[at:]
			parts := strings.Fields(line)
			for i, part := range parts {
				if part == "as" && i+1 < len(parts) {
					return parts[i+1], nil
				}
			}
		}
	}
	return "", fmt.Errorf("could not parse replay session id from stderr: %s", stderr)
}

func waitForFile(path string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if _, err := os.Stat(path); err == nil {
			return nil
		}
		time.Sleep(20 * time.Millisecond)
	}
	return fmt.Errorf("marker not created: %s", path)
}
