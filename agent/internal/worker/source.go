package worker

import (
	"context"
	"errors"
	"fmt"
	pb "github.com/zhubby/artifact-kit/agent/gen/agentv1"
	"github.com/zhubby/artifact-kit/agent/internal/config"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
	"time"
)

var sourceID = regexp.MustCompile(`^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$`)
var sourceCommit = regexp.MustCompile(`^[0-9a-f]{40}([0-9a-f]{24})?$`)

// AI tasks inherit toolchain locations, not arbitrary daemon credentials.
func buildEnvironment() []string {
	result := []string{}
	for _, key := range []string{"PATH", "HOME", "USER", "LOGNAME", "SHELL", "LANG", "LC_ALL", "TZ", "TMPDIR", "TMP", "TEMP", "SystemRoot", "SYSTEMROOT", "WINDIR", "COMSPEC", "PATHEXT", "USERPROFILE", "APPDATA", "LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)", "JAVA_HOME", "ANDROID_HOME", "ANDROID_SDK_ROOT", "DEVELOPER_DIR", "SDKROOT", "CARGO_HOME", "RUSTUP_HOME", "GOPATH", "GOCACHE", "CC", "CXX", "PKG_CONFIG_PATH", "HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY", "SSL_CERT_FILE", "SSL_CERT_DIR", "NODE_EXTRA_CA_CERTS"} {
		if value, ok := os.LookupEnv(key); ok {
			result = append(result, key+"="+value)
		}
	}
	return result
}

func sourceEnvironment(input Input) ([]string, error) {
	executable, err := os.Executable()
	if err != nil {
		return nil, err
	}
	environment := append(buildEnvironment(), "GIT_TERMINAL_PROMPT=0", "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull, "GIT_LFS_SKIP_SMUDGE=1", "GIT_ASKPASS="+executable, "ARTIFACTORY_GIT_ASKPASS=1", "ARTIFACTORY_GIT_USERNAME=artifactory", "ARTIFACTORY_GIT_PASSWORD="+input.Assignment.Source.AccessToken)
	if input.Config.GitCAFile != "" {
		environment = append(environment, "GIT_SSL_CAINFO="+input.Config.GitCAFile)
	}
	return environment, nil
}

func sourceGit(ctx context.Context, environment []string, emit *emitter, arguments ...string) error {
	args := append([]string{"-c", "credential.helper=", "-c", "core.hooksPath=" + os.DevNull, "-c", "http.followRedirects=false"}, arguments...)
	command := exec.Command("git", args...)
	command.Env = environment
	command.Stdout = logWriter{emit, "checkout"}
	command.Stderr = command.Stdout
	limited, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()
	if err := runCommand(limited, command); err != nil {
		return errors.New("platform source operation failed; check source access, Git/LFS tools and control-plane reachability")
	}
	return nil
}

func managedSourcePath(root, relative string) (string, error) {
	if relative == "" {
		return root, nil
	}
	if strings.ContainsAny(relative, "\\:\x00\r\n") || strings.HasPrefix(relative, "/") {
		return "", errors.New("unsafe source path")
	}
	current := root
	for _, part := range strings.Split(relative, "/") {
		if part == "" || part == "." || part == ".." || strings.EqualFold(part, ".git") {
			return "", errors.New("unsafe source path")
		}
		current = filepath.Join(current, part)
		info, err := os.Lstat(current)
		if err == nil && info.Mode()&os.ModeSymlink != 0 {
			return "", errors.New("source path crosses a symbolic link")
		}
		if err != nil && !os.IsNotExist(err) {
			return "", err
		}
	}
	return current, nil
}

func checkoutPlatform(ctx context.Context, input Input, base, workspace string, emit *emitter) error {
	source := input.Assignment.Source
	if !sourceID.MatchString(source.SnapshotId) || len(source.AccessToken) < 32 || len(source.Repositories) == 0 {
		return errors.New("invalid source descriptor")
	}
	environment, err := sourceEnvironment(input)
	if err != nil {
		return err
	}
	repositories := append([]*pb.SourceRepository(nil), source.Repositories...)
	sort.Slice(repositories, func(i, j int) bool { return repositories[i].Path < repositories[j].Path })
	if repositories[0].Path != "" || repositories[0].CommitSha != input.Assignment.CommitSha {
		return errors.New("source root does not match pinned commit")
	}
	seen := map[string]bool{}
	for _, repository := range repositories {
		if !sourceID.MatchString(repository.CacheId) || !sourceID.MatchString(repository.RepositoryId) || !sourceCommit.MatchString(repository.CommitSha) || seen[repository.Path] {
			return errors.New("invalid source repository")
		}
		seen[repository.Path] = true
		if !strings.HasPrefix(repository.GitUrl, "https://") && !(input.Config.AllowHTTPUploads && strings.HasPrefix(repository.GitUrl, "http://")) {
			return errors.New("HTTPS platform source URL required")
		}
		target, err := managedSourcePath(workspace, repository.Path)
		if err != nil {
			return err
		}
		cacheRoot := filepath.Join(input.Config.Root, "cache", "git")
		if err = os.MkdirAll(cacheRoot, 0700); err != nil {
			return err
		}
		unlock, err := lockSourceCache(ctx, filepath.Join(cacheRoot, repository.CacheId+".lock"))
		if err != nil {
			return err
		}
		err = checkoutRepository(ctx, input, repository, environment, cacheRoot, target, emit)
		unlock()
		if err != nil {
			return err
		}
	}
	// Keep repository files unchanged while making ordinary Git submodule
	// commands use the task-scoped platform endpoints as well.
	for _, parent := range repositories {
		target, err := managedSourcePath(workspace, parent.Path)
		if err != nil {
			return err
		}
		if _, err = os.Stat(filepath.Join(target, ".gitmodules")); os.IsNotExist(err) {
			continue
		}
		command := exec.CommandContext(ctx, "git", "-C", target, "config", "--null", "--file", ".gitmodules", "--get-regexp", `^submodule\..*\.path$`)
		command.Env = environment
		output, err := command.Output()
		if err != nil {
			var exit *exec.ExitError
			if !errors.As(err, &exit) || exit.ExitCode() != 1 {
				return errors.New("cannot read snapshot submodule paths")
			}
		}
		for _, item := range strings.Split(string(output), "\x00") {
			if item == "" {
				continue
			}
			key, relative, ok := strings.Cut(item, "\n")
			if !ok {
				return errors.New("invalid submodule path entry")
			}
			childPath := relative
			if parent.Path != "" {
				childPath = parent.Path + "/" + relative
			}
			for _, child := range repositories {
				if child.Path == childPath {
					name := strings.TrimSuffix(key, ".path")
					if err = sourceGit(ctx, environment, emit, "-C", target, "config", "--local", name+".url", child.GitUrl); err != nil {
						return err
					}
					break
				}
			}
		}
	}
	_ = collectSourceCache(ctx, input.Config)
	return nil
}

func collectSourceCache(ctx context.Context, cfg config.Config) error {
	quota := cfg.SourceCacheBytes
	if quota == 0 {
		quota = 20 * 1024 * 1024 * 1024
	}
	root := filepath.Join(cfg.Root, "cache", "git")
	entries, err := os.ReadDir(root)
	if err != nil {
		return err
	}
	type cached struct {
		path, id string
		bytes    int64
		used     time.Time
	}
	caches := []cached{}
	var total int64
	for _, entry := range entries {
		id := strings.TrimSuffix(entry.Name(), ".git")
		if !entry.IsDir() || !sourceID.MatchString(id) {
			continue
		}
		path := filepath.Join(root, entry.Name())
		info, err := entry.Info()
		if err != nil {
			return err
		}
		var size int64
		err = filepath.WalkDir(path, func(_ string, item os.DirEntry, err error) error {
			if err != nil {
				return err
			}
			if !item.IsDir() {
				info, err := item.Info()
				if err != nil {
					return err
				}
				size += info.Size()
			}
			return nil
		})
		if err != nil {
			return err
		}
		caches = append(caches, cached{path, id, size, info.ModTime()})
		total += size
	}
	sort.Slice(caches, func(i, j int) bool { return caches[i].used.Before(caches[j].used) })
	for _, cache := range caches {
		if total <= quota {
			break
		}
		// A short lock attempt skips caches being used by other tasks.
		limited, cancel := context.WithTimeout(ctx, 50*time.Millisecond)
		unlock, err := lockSourceCache(limited, filepath.Join(root, cache.id+".lock"))
		cancel()
		if err != nil {
			continue
		}
		err = os.RemoveAll(cache.path)
		unlock()
		if err != nil {
			return err
		}
		total -= cache.bytes
	}
	return nil
}

func checkoutRepository(ctx context.Context, input Input, repository *pb.SourceRepository, environment []string, cacheRoot, target string, emit *emitter) error {
	cache := filepath.Join(cacheRoot, repository.CacheId+".git")
	if _, err := os.Stat(filepath.Join(cache, "HEAD")); os.IsNotExist(err) {
		if err = sourceGit(ctx, environment, emit, "init", "--bare", cache); err != nil {
			return err
		}
	}
	probe := exec.CommandContext(ctx, "git", "-C", cache, "cat-file", "-e", repository.CommitSha+"^{commit}")
	probe.Env = environment
	cached := probe.Run() == nil
	if cached {
		for _, oid := range repository.Tags {
			if !sourceCommit.MatchString(oid) {
				return errors.New("invalid source tag object")
			}
			command := exec.CommandContext(ctx, "git", "-C", cache, "cat-file", "-e", oid)
			command.Env = environment
			if command.Run() != nil {
				cached = false
				break
			}
		}
	}
	if !cached {
		if err := sourceGit(ctx, environment, emit, "-C", cache, "fetch", "--force", "--tags", "--no-recurse-submodules", "--", repository.GitUrl, "refs/heads/source"); err != nil {
			return err
		}
	}
	emit.emit(Event{Kind: "source_cache", Payload: map[string]any{"path": repository.Path, "hit": cached, "commit": repository.CommitSha}})
	if err := sourceGit(ctx, environment, emit, "-C", cache, "update-ref", "refs/heads/source", repository.CommitSha); err != nil {
		return err
	}
	if repository.HasLfs {
		if repository.LfsUrl != strings.TrimSuffix(repository.GitUrl, "/git")+"/lfs" {
			return errors.New("invalid platform LFS endpoint")
		}
		if err := sourceGit(ctx, environment, emit, "-C", cache, "-c", "remote.origin.url="+repository.GitUrl, "-c", "lfs.url="+repository.LfsUrl, "lfs", "fetch", "origin", repository.CommitSha); err != nil {
			return err
		}
	}
	if err := os.MkdirAll(filepath.Dir(target), 0700); err != nil {
		return err
	}
	if err := sourceGit(ctx, environment, emit, "clone", "--local", "--no-hardlinks", "--no-checkout", "--", cache, target); err != nil {
		return err
	}
	if err := sourceGit(ctx, environment, emit, "-C", target, "remote", "set-url", "origin", repository.GitUrl); err != nil {
		return err
	}
	output, err := exec.CommandContext(ctx, "git", "-C", target, "tag", "--list").Output()
	if err != nil {
		return err
	}
	for _, name := range strings.Split(strings.TrimSpace(string(output)), "\n") {
		if name != "" {
			if err := sourceGit(ctx, environment, emit, "-C", target, "update-ref", "-d", "refs/tags/"+name); err != nil {
				return err
			}
		}
	}
	for name, oid := range repository.Tags {
		if !sourceCommit.MatchString(oid) || strings.HasPrefix(name, "-") {
			return errors.New("invalid frozen tag")
		}
		if err := sourceGit(ctx, environment, emit, "-C", target, "update-ref", "refs/tags/"+name, oid); err != nil {
			return err
		}
	}
	if repository.HasLfs {
		// Working copies never write into the persistent LFS cache.
		if err := copyLFS(filepath.Join(cache, "lfs", "objects"), filepath.Join(target, ".git", "lfs", "objects")); err != nil {
			return err
		}
		settings := map[string]string{"lfs.url": repository.LfsUrl, "filter.lfs.clean": "git-lfs clean -- %f", "filter.lfs.smudge": "git-lfs smudge -- %f", "filter.lfs.process": "git-lfs filter-process", "filter.lfs.required": "true"}
		for key, value := range settings {
			if err := sourceGit(ctx, environment, emit, "-C", target, "config", key, value); err != nil {
				return err
			}
		}
	}
	if err := sourceGit(ctx, environment, emit, "-C", target, "checkout", "--detach", repository.CommitSha); err != nil {
		return err
	}
	if repository.HasLfs {
		if err := sourceGit(ctx, environment, emit, "-C", target, "lfs", "checkout"); err != nil {
			return err
		}
		if err := sourceGit(ctx, environment, emit, "-C", target, "lfs", "fsck", "--objects", repository.CommitSha); err != nil {
			return err
		}
	}
	_ = os.Chtimes(cache, time.Now(), time.Now())
	return nil
}

func copyLFS(source, destination string) error {
	return filepath.WalkDir(source, func(path string, entry os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		relative, err := filepath.Rel(source, path)
		if err != nil {
			return err
		}
		output := filepath.Join(destination, relative)
		if entry.IsDir() {
			return os.MkdirAll(output, 0700)
		}
		if !entry.Type().IsRegular() {
			return errors.New("invalid LFS cache entry")
		}
		input, err := os.Open(path)
		if err != nil {
			return err
		}
		defer input.Close()
		file, err := os.OpenFile(output, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0600)
		if err != nil {
			return err
		}
		_, err = io.Copy(file, input)
		closeErr := file.Close()
		if err != nil {
			return err
		}
		return closeErr
	})
}

func verifySource(ctx context.Context, input Input, workspace string) error {
	if input.Assignment.Source == nil {
		return nil
	}
	for _, repository := range input.Assignment.Source.Repositories {
		target, err := managedSourcePath(workspace, repository.Path)
		if err != nil {
			return err
		}
		command := exec.CommandContext(ctx, "git", "-c", "core.hooksPath="+os.DevNull, "-c", "core.filemode=true", "-c", "core.fsmonitor=false", "-C", target, "diff", "--exit-code", "--no-ext-diff", "--ignore-submodules=untracked", "HEAD", "--")
		command.Env = append(os.Environ(), "GIT_CONFIG_NOSYSTEM=1", "GIT_CONFIG_GLOBAL="+os.DevNull)
		if command.Run() != nil {
			if ctx.Err() != nil {
				return ctx.Err()
			}
			return fmt.Errorf("source_modified: tracked files or submodule state changed in %q; use external build output", repository.Path)
		}
		head, err := exec.CommandContext(ctx, "git", "-C", target, "rev-parse", "HEAD").Output()
		if err != nil || strings.TrimSpace(string(head)) != repository.CommitSha {
			return errors.New("source_modified: pinned source commit changed")
		}
	}
	return nil
}
