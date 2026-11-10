package client

import (
	"bufio"
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"net/http/cookiejar"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
	"time"

	pb "github.com/artifact-kit/agent/gen/agentv1"
	"github.com/artifact-kit/agent/internal/config"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
)

func TestLiveRustGoProtocol(test *testing.T) {
	if os.Getenv("AK_LIVE_TESTS") != "1" {
		test.Skip("requires live Rust control, PostgreSQL, S3 and scripts/acceptance.py --keep-alive")
	}
	base := os.Getenv("AK_LIVE_HTTP_URL")
	if base == "" {
		base = "http://127.0.0.1:8080"
	}
	jar, _ := cookiejar.New(nil)
	browser := &http.Client{Jar: jar, Timeout: 30 * time.Second}
	call := func(method, path string, body any) map[string]any {
		test.Helper()
		encoded, _ := json.Marshal(body)
		request, err := http.NewRequest(method, base+"/api/v1"+path, bytes.NewReader(encoded))
		if err != nil {
			test.Fatal(err)
		}
		request.Header.Set("Content-Type", "application/json")
		request.Header.Set("x-artifact-csrf", "1")
		response, err := browser.Do(request)
		if err != nil {
			test.Fatal(err)
		}
		defer response.Body.Close()
		if response.StatusCode != 200 {
			test.Fatalf("%s %s: HTTP %d", method, path, response.StatusCode)
		}
		var result map[string]any
		if err = json.NewDecoder(response.Body).Decode(&result); err != nil {
			test.Fatal(err)
		}
		return result
	}
	call("POST", "/auth/login", map[string]string{"username": os.Getenv("AK_BOOTSTRAP_USERNAME"), "password": os.Getenv("AK_BOOTSTRAP_PASSWORD")})
	existing, err := config.Load(os.Getenv("AK_LIVE_AGENT_CONFIG"))
	if err != nil {
		test.Fatal(err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 60*time.Second)
	defer cancel()
	anonymous, err := Dial(existing, true)
	if err != nil {
		test.Fatal(err)
	}
	defer anonymous.Close()
	_, err = pb.NewControlClient(anonymous).PrepareArtifact(ctx, &pb.ArtifactRequest{})
	if status.Code(err) != codes.Unauthenticated {
		test.Fatalf("no-certificate RPC: %v", err)
	}
	_, err = pb.NewEnrollmentClient(anonymous).Enroll(ctx, &pb.EnrollRequest{ProtocolMajor: 999, Token: strings.Repeat("0", 64)})
	if status.Code(err) != codes.FailedPrecondition {
		test.Fatalf("unsupported protocol: %v", err)
	}
	pool := call("POST", "/pools", map[string]string{"name": fmt.Sprintf("protocol-%d", time.Now().UnixNano())})["id"].(string)
	token := call("POST", "/enrollment-tokens", map[string]string{"pool_id": pool})["token"].(string)
	directory := test.TempDir()
	cfg := config.Config{Endpoint: existing.Endpoint, CAFile: existing.CAFile, Root: directory, Name: "protocol-probe"}
	path := filepath.Join(directory, "agent.json")
	if err = Enroll(ctx, path, cfg, token); err != nil {
		test.Fatal(err)
	}
	cfg, err = config.Load(path)
	if err != nil {
		test.Fatal(err)
	}
	source := call("GET", "/projects/"+os.Getenv("AK_LIVE_PROJECT_ID"), nil)
	profiles := source["signing_profiles"]
	nodeSettings := map[string]any{"draining": false, "revoked": false, "slots": 1, "labels": map[string]string{}, "signing_profiles": profiles, "pool_id": pool}
	call("PATCH", "/nodes/"+cfg.AgentID, nodeSettings)
	project := call("POST", "/projects", map[string]any{"name": "Protocol acceptance", "repository_url": source["repository_url"], "credential_id": source["credential_id"], "signing_profiles": profiles, "pool_ids": []string{pool}})["id"].(string)
	connection, err := Dial(cfg, false)
	if err != nil {
		test.Fatal(err)
	}
	defer connection.Close()
	control := pb.NewControlClient(connection)
	bad, err := control.Connect(ctx)
	if err != nil {
		test.Fatal(err)
	}
	if err = bad.Send(&pb.AgentEvent{AgentId: cfg.AgentID, Kind: "hello", ProtocolMajor: 999}); err != nil {
		test.Fatal(err)
	}
	_, err = bad.Recv()
	if status.Code(err) != codes.FailedPrecondition {
		test.Fatalf("incompatible hello: %v", err)
	}
	stream, err := control.Connect(ctx)
	if err != nil {
		test.Fatal(err)
	}
	hello := &pb.AgentEvent{AgentId: cfg.AgentID, Kind: "hello", ProtocolMajor: 1, PayloadJson: `{"capabilities":["git","bash","sh","cc","codesign"]}`}
	if err = stream.Send(hello); err != nil {
		test.Fatal(err)
	}
	build := call("POST", "/projects/"+project+"/builds", map[string]any{"ref": "main", "targets": []string{"native"}})["id"].(string)
	assignment, err := stream.Recv()
	if err != nil {
		test.Fatal(err)
	}
	if assignment.Kind != "assign" {
		test.Fatalf("expected assignment, got %s", assignment.Kind)
	}
	accept := &pb.AgentEvent{AgentId: cfg.AgentID, Kind: "accept", AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, CommandId: "duplicate-accept"}
	for repeat := 0; repeat < 2; repeat++ {
		if err = stream.Send(accept); err != nil {
			test.Fatal(err)
		}
		grant, err := stream.Recv()
		if err != nil {
			test.Fatal(err)
		}
		if grant.Kind != "start" || grant.RequestId != accept.CommandId {
			test.Fatal("acceptance replay did not produce matching start grant")
		}
	}
	probeStorage := func(name string, operation func(context.Context) error) {
		test.Helper()
		container := os.Getenv("AK_LIVE_S3_CONTAINER")
		if container == "" {
			return
		}
		if output, pauseError := exec.Command("docker", "pause", container).CombinedOutput(); pauseError != nil {
			test.Fatalf("pause dedicated S3 container: %v: %s", pauseError, output)
		}
		paused := true
		defer func() {
			if paused {
				if output, resumeError := exec.Command("docker", "unpause", container).CombinedOutput(); resumeError != nil {
					test.Errorf("resume dedicated S3 container: %v: %s", resumeError, output)
				}
			}
		}()
		probeContext, stop := context.WithTimeout(ctx, 20*time.Second)
		defer stop()
		completed := make(chan error, 1)
		go func() { completed <- operation(probeContext) }()
		select {
		case operationError := <-completed:
			test.Fatalf("%s storage request did not stall: %v", name, operationError)
		case <-time.After(750 * time.Millisecond):
		}
		payload, _ := json.Marshal(map[string]any{"capabilities": []string{"git", "bash", "sh", "cc", "codesign"}, "active_attempts": []map[string]string{{"attempt_id": assignment.AttemptId, "lease_token": assignment.LeaseToken}}})
		requestID := "storage-stall-" + name
		if sendError := stream.Send(&pb.AgentEvent{AgentId: cfg.AgentID, Kind: "heartbeat", CommandId: requestID, PayloadJson: string(payload)}); sendError != nil {
			test.Fatal(sendError)
		}
		granted := make(chan *pb.Command, 1)
		failed := make(chan error, 1)
		go func() {
			grant, receiveError := stream.Recv()
			if receiveError != nil {
				failed <- receiveError
				return
			}
			granted <- grant
		}()
		select {
		case grant := <-granted:
			if grant.Kind != "renew" || grant.RequestId != requestID || grant.AttemptId != assignment.AttemptId {
				test.Fatalf("%s storage stall returned an invalid lease grant", name)
			}
		case receiveError := <-failed:
			test.Fatal(receiveError)
		case <-time.After(3 * time.Second):
			test.Fatalf("%s storage stall blocked the independent heartbeat", name)
		}
		if output, resumeError := exec.Command("docker", "unpause", container).CombinedOutput(); resumeError != nil {
			test.Fatalf("resume dedicated S3 container: %v: %s", resumeError, output)
		}
		paused = false
		if operationError := <-completed; operationError != nil {
			test.Fatalf("%s storage request failed after recovery: %v", name, operationError)
		}
		test.Log(name + " S3 stall did not block heartbeat renewal")
	}
	probeStorage("log", func(probeContext context.Context) error {
		logConnection, dialError := Dial(cfg, false)
		if dialError != nil {
			return dialError
		}
		defer logConnection.Close()
		upload, uploadError := pb.NewControlClient(logConnection).UploadLogs(probeContext)
		if uploadError != nil {
			return uploadError
		}
		if uploadError = upload.Send(&pb.LogChunk{AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, Sequence: 1, Step: "protocol", Data: []byte("first")}); uploadError != nil {
			return uploadError
		}
		_, uploadError = upload.CloseAndRecv()
		return uploadError
	})
	sendLog := func(sequence uint64, content string) codes.Code {
		test.Helper()
		upload, err := control.UploadLogs(ctx)
		if err != nil {
			test.Fatal(err)
		}
		if err = upload.Send(&pb.LogChunk{AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, Sequence: sequence, Step: "protocol", Data: []byte(content)}); err != nil {
			test.Fatal(err)
		}
		ack, err := upload.CloseAndRecv()
		if err == nil && ack.ThroughSequence != sequence {
			test.Fatal("incorrect log acknowledgement")
		}
		return status.Code(err)
	}
	if sendLog(1, "first") != codes.OK || sendLog(1, "first") != codes.OK || sendLog(1, "different") != codes.AlreadyExists || sendLog(3, "gap") != codes.FailedPrecondition || sendLog(2, "second") != codes.OK {
		test.Fatal("log replay/order contract violated")
	}
	replayContext, stopReplay := context.WithTimeout(ctx, 5*time.Second)
	defer stopReplay()
	replayRequest, err := http.NewRequestWithContext(replayContext, "GET", base+"/api/v1/builds/"+build+"/events", nil)
	if err != nil {
		test.Fatal(err)
	}
	replayResponse, err := browser.Do(replayRequest)
	if err != nil {
		test.Fatal(err)
	}
	var replayed []string
	scanner := bufio.NewScanner(replayResponse.Body)
	scanner.Buffer(make([]byte, 4096), 4*1024*1024)
	for scanner.Scan() {
		line := scanner.Text()
		if !strings.HasPrefix(line, "data:") {
			continue
		}
		var event map[string]any
		if json.Unmarshal([]byte(strings.TrimSpace(strings.TrimPrefix(line, "data:"))), &event) == nil {
			if text, exists := event["text"].(string); exists {
				replayed = append(replayed, text)
				if len(replayed) == 2 {
					break
				}
			}
		}
	}
	replayResponse.Body.Close()
	if strings.Join(replayed, ",") != "first,second" {
		test.Fatalf("conflicting replay mutated an accepted log: %v", replayed)
	}
	data := []byte("verified immutable artifact")
	digest := sha256.Sum256(data)
	declaration := &pb.ArtifactRequest{AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, Path: "dist/probe", Size: uint64(len(data)), Sha256: hex.EncodeToString(digest[:])}
	ticket, err := control.PrepareArtifact(ctx, declaration)
	if err != nil {
		test.Fatal(err)
	}
	put := func(data []byte) {
		test.Helper()
		request, err := http.NewRequestWithContext(ctx, "PUT", ticket.Url, bytes.NewReader(data))
		if err != nil {
			test.Fatal(err)
		}
		response, err := http.DefaultClient.Do(request)
		if err != nil {
			test.Fatal(err)
		}
		response.Body.Close()
		if response.StatusCode/100 != 2 {
			test.Fatalf("S3 upload: HTTP %d", response.StatusCode)
		}
	}
	complete := &pb.CompleteArtifactRequest{AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, UploadId: ticket.UploadId}
	put(bytes.Repeat([]byte("x"), len(data)))
	if _, err = control.CompleteArtifact(ctx, complete); status.Code(err) != codes.FailedPrecondition {
		test.Fatalf("accepted corrupt artifact: %v", err)
	}
	put(data)
	probeStorage("artifact", func(probeContext context.Context) error {
		_, completeError := control.CompleteArtifact(probeContext, complete)
		return completeError
	})
	artifact, err := control.CompleteArtifact(ctx, complete)
	if err != nil {
		test.Fatal(err)
	}
	repeated, err := control.CompleteArtifact(ctx, complete)
	if err != nil || repeated.ArtifactId != artifact.ArtifactId {
		test.Fatal("artifact completion not idempotent")
	}
	put(bytes.Repeat([]byte("z"), len(data)))
	response, err := browser.Get(base + "/api/v1/artifacts/" + artifact.ArtifactId + "/download")
	if err != nil {
		test.Fatal(err)
	}
	downloaded, err := io.ReadAll(response.Body)
	response.Body.Close()
	if err != nil || !bytes.Equal(downloaded, data) {
		test.Fatal("staging PUT modified the verified immutable object")
	}
	call("POST", "/builds/"+build+"/cancel", map[string]string{})
	if _, err = control.PrepareArtifact(ctx, declaration); status.Code(err) != codes.FailedPrecondition {
		test.Fatalf("canceled artifact accepted: %v", err)
	}
	finished := &pb.AgentEvent{AgentId: cfg.AgentID, Kind: "finished", AttemptId: assignment.AttemptId, LeaseToken: assignment.LeaseToken, Sequence: 1, PayloadJson: `{"status":"failed","error":"protocol-only probe; no build executed"}`}
	for repeat := 0; repeat < 2; repeat++ {
		if err = stream.Send(finished); err != nil {
			test.Fatal(err)
		}
		for {
			reply, err := stream.Recv()
			if err != nil {
				test.Fatal(err)
			}
			if reply.Kind == "event_ack" {
				if reply.AckSequence != 1 {
					test.Fatal("invalid replay ack")
				}
				break
			}
		}
	}
	finished.Sequence = 2
	finished.PayloadJson = `{"status":"succeeded"}`
	if err = stream.Send(finished); err != nil {
		test.Fatal(err)
	}
	for {
		_, err = stream.Recv()
		if err != nil {
			break
		}
	}
	if status.Code(err) != codes.FailedPrecondition {
		test.Fatalf("late result not rejected: %v", err)
	}
	detail := call("GET", "/builds/"+build, nil)
	jobs := detail["jobs"].([]any)
	if jobs[0].(map[string]any)["status"] != "canceled" {
		test.Fatal("late success changed canceled job")
	}
	nodeSettings["revoked"] = true
	call("PATCH", "/nodes/"+cfg.AgentID, nodeSettings)
	nodeSettings["revoked"] = false
	call("PATCH", "/nodes/"+cfg.AgentID, nodeSettings)
	if _, err = control.PrepareArtifact(ctx, declaration); status.Code(err) != codes.Unauthenticated {
		test.Fatalf("revoked certificate accepted: %v", err)
	}
	test.Log("live checks: mTLS, version negotiation, one-use enrollment, duplicate accept/events, ordered/replayed logs, corrupt upload rejection, immutable promotion, cancellation, late result fencing, irreversible revocation")
}
