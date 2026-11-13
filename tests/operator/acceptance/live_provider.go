// Optional transparent supplier relay for this isolated acceptance fixture.
// It never stores credentials, request bodies or generated argument contents.
package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"io"
	"net/http"
	"net/url"
	"os"
	"strings"
	"sync"
	"time"
)

type supplierMode struct {
	Scenario string `json:"scenario"`
	Reject   int    `json:"reject"`
	Hold     bool   `json:"hold_after_arguments"`
}
type supplierRequest struct {
	Index            int      `json:"index"`
	Started          int64    `json:"started_ms"`
	Sent             int64    `json:"sent_ms"`
	Headers          int64    `json:"headers_ms"`
	Finished         int64    `json:"finished_ms"`
	Status           int      `json:"status"`
	Injected         bool     `json:"injected"`
	Streaming        bool     `json:"streaming"`
	ContentType      string   `json:"content_type"`
	InputToolResults int      `json:"input_tool_results"`
	Events           int      `json:"events"`
	ArgumentDeltas   int      `json:"argument_deltas"`
	ArgumentBytes    int      `json:"argument_bytes"`
	Tools            []string `json:"tools"`
	Done             bool     `json:"done"`
	Held             bool     `json:"held"`
	Disconnected     bool     `json:"client_disconnected"`
}
type supplierScenario struct {
	Mode     supplierMode       `json:"mode"`
	Requests []*supplierRequest `json:"requests"`
}

func mountLiveProvider(mux *http.ServeMux) {
	base := strings.TrimSuffix(os.Getenv("LIVE_PROVIDER_BASE_URL"), "/")
	if base == "" {
		return
	}
	u, err := url.Parse(base)
	if err != nil || u.Scheme != "https" || u.Host != "dashscope.aliyuncs.com" || u.Path != "/compatible-mode/v1" || u.RawQuery != "" || u.User != nil {
		panic("live acceptance supplier must be the fixed DashScope HTTPS endpoint")
	}
	client := &http.Client{Timeout: 610 * time.Second, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	var mu sync.Mutex
	scenarios := map[string]*supplierScenario{}
	mux.HandleFunc("/test/provider", func(w http.ResponseWriter, r *http.Request) {
		mu.Lock()
		defer mu.Unlock()
		w.Header().Set("Content-Type", "application/json")
		if r.Method == http.MethodGet {
			_ = json.NewEncoder(w).Encode(scenarios)
			return
		}
		var mode supplierMode
		if json.NewDecoder(http.MaxBytesReader(w, r.Body, 2048)).Decode(&mode) != nil || len(mode.Scenario) < 1 || len(mode.Scenario) > 80 || strings.ContainsAny(mode.Scenario, "/? ") || mode.Reject < 0 || mode.Reject > 2 || len(scenarios) >= 32 || scenarios[mode.Scenario] != nil {
			http.Error(w, "invalid or reused scenario", 400)
			return
		}
		scenarios[mode.Scenario] = &supplierScenario{Mode: mode, Requests: []*supplierRequest{}}
		_, _ = w.Write([]byte("{}"))
	})
	mux.HandleFunc("/provider/", func(w http.ResponseWriter, r *http.Request) {
		parts := strings.Split(strings.TrimPrefix(r.URL.Path, "/provider/"), "/")
		if r.Method != http.MethodPost || len(parts) != 4 || strings.Join(parts[1:], "/") != "v1/chat/completions" {
			http.NotFound(w, r)
			return
		}
		body, err := io.ReadAll(http.MaxBytesReader(w, r.Body, 2<<20))
		var input struct {
			Stream   bool `json:"stream"`
			Messages []struct {
				Role string `json:"role"`
			} `json:"messages"`
		}
		if err != nil || json.Unmarshal(body, &input) != nil || !input.Stream {
			http.Error(w, "expected streaming request", 400)
			return
		}
		mu.Lock()
		scenario := scenarios[parts[0]]
		if scenario == nil || len(scenario.Requests) >= 32 {
			mu.Unlock()
			http.Error(w, "scenario unavailable", 429)
			return
		}
		record := &supplierRequest{Index: len(scenario.Requests) + 1, Started: time.Now().UnixMilli(), Streaming: input.Stream, Tools: []string{}}
		for _, m := range input.Messages {
			if m.Role == "tool" {
				record.InputToolResults++
			}
		}
		injected := record.Index <= scenario.Mode.Reject
		hold := scenario.Mode.Hold
		scenario.Requests = append(scenario.Requests, record)
		mu.Unlock()
		update := func(f func()) { mu.Lock(); defer mu.Unlock(); f() }
		defer update(func() { record.Finished = time.Now().UnixMilli(); record.Disconnected = r.Context().Err() != nil })
		if injected {
			update(func() { record.Status = 429; record.Injected = true })
			w.Header().Set("Content-Type", "application/json")
			w.Header().Set("Retry-After", "2")
			w.WriteHeader(429)
			_, _ = w.Write([]byte(`{"error":{"code":"rate_limit_exceeded","message":"controlled acceptance 429"}}`))
			return
		}
		request, err := http.NewRequestWithContext(r.Context(), http.MethodPost, base+"/chat/completions", bytes.NewReader(body))
		if err != nil {
			http.Error(w, "relay request failed", 502)
			return
		}
		request.Header.Set("Authorization", r.Header.Get("Authorization"))
		request.Header.Set("Content-Type", "application/json")
		update(func() { record.Sent = time.Now().UnixMilli() })
		response, err := client.Do(request)
		if err != nil {
			http.Error(w, "supplier transport failed", 502)
			return
		}
		defer response.Body.Close()
		update(func() {
			record.Headers = time.Now().UnixMilli()
			record.Status = response.StatusCode
			record.ContentType = response.Header.Get("Content-Type")
		})
		for _, name := range []string{"Content-Type", "Retry-After"} {
			if value := response.Header.Get(name); value != "" {
				w.Header().Set(name, value)
			}
		}
		w.WriteHeader(response.StatusCode)
		reader := bufio.NewScanner(io.LimitReader(response.Body, 8<<20))
		reader.Buffer(make([]byte, 64<<10), 2<<20)
		flusher, _ := w.(http.Flusher)
		for reader.Scan() {
			line := reader.Bytes()
			argument := false
			if bytes.HasPrefix(line, []byte("data: ")) {
				data := bytes.TrimSpace(line[6:])
				update(func() {
					record.Events++
					if bytes.Equal(data, []byte("[DONE]")) {
						record.Done = true
					}
				})
				var event struct {
					Choices []struct {
						Delta struct {
							ToolCalls []struct {
								Function struct {
									Name      string `json:"name"`
									Arguments string `json:"arguments"`
								} `json:"function"`
							} `json:"tool_calls"`
						} `json:"delta"`
					} `json:"choices"`
				}
				if json.Unmarshal(data, &event) == nil {
					for _, choice := range event.Choices {
						for _, tool := range choice.Delta.ToolCalls {
							update(func() {
								if tool.Function.Name != "" {
									record.Tools = append(record.Tools, tool.Function.Name)
								}
								if tool.Function.Arguments != "" {
									record.ArgumentDeltas++
									record.ArgumentBytes += len(tool.Function.Arguments)
									argument = true
								}
							})
						}
					}
				}
			}
			if _, err = w.Write(append(append([]byte{}, line...), '\n')); err != nil {
				return
			}
			if flusher != nil {
				flusher.Flush()
			}
			if hold && argument {
				// Finish this SSE event before waiting so the downstream receives a real delta.
				_, _ = w.Write([]byte("\n"))
				if flusher != nil {
					flusher.Flush()
				}
				update(func() { record.Held = true })
				<-r.Context().Done()
				return
			}
		}
	})
}
