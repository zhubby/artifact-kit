package executor

import (
	"errors"
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"sort"
	"strings"

	"github.com/bmatcuk/doublestar/v4"
)

func SafeRelative(path string) bool {
	if path == "" || len(path) > 1024 || strings.ContainsAny(path, "\\:\x00") || strings.HasPrefix(path, "/") {
		return false
	}
	for _, part := range strings.Split(path, "/") {
		if part == "" || part == ".." || part == "." {
			return false
		}
	}
	return true
}

func Collect(root string, patterns []string) ([]string, error) {
	for _, pattern := range patterns {
		if !SafeRelative(pattern) || !doublestar.ValidatePattern(pattern) {
			return nil, errors.New("unsafe artifact pattern")
		}
	}
	matches := make([]int, len(patterns))
	var files []string
	err := filepath.WalkDir(root, func(path string, entry fs.DirEntry, walkError error) error {
		if walkError != nil {
			return walkError
		}
		relative, err := filepath.Rel(root, path)
		if err != nil {
			return err
		}
		relative = filepath.ToSlash(relative)
		if entry.IsDir() {
			if relative == ".git" {
				return filepath.SkipDir
			}
			return nil
		}
		matched := false
		for index, pattern := range patterns {
			match, err := doublestar.Match(pattern, relative)
			if err != nil {
				return err
			}
			if match {
				if !entry.Type().IsRegular() {
					return fmt.Errorf("artifact is not a regular file: %s", relative)
				}
				matches[index]++
				matched = true
			}
		}
		if matched {
			files = append(files, relative)
		}
		if entry.Type()&os.ModeSymlink != 0 {
			for _, pattern := range patterns {
				prefix := strings.Split(pattern, "*")[0]
				if strings.HasPrefix(prefix, relative+"/") {
					return fmt.Errorf("artifact directory is a symlink: %s", relative)
				}
			}
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	for index, count := range matches {
		if count == 0 {
			return nil, fmt.Errorf("required artifact pattern matched no files: %s", patterns[index])
		}
	}
	sort.Strings(files)
	return files, nil
}

func OpenArtifact(root, path string) (*os.File, error) {
	if !SafeRelative(path) {
		return nil, errors.New("unsafe artifact path")
	}
	directory, err := os.OpenRoot(root)
	if err != nil {
		return nil, err
	}
	defer directory.Close()
	parts := strings.Split(path, "/")
	for index := range parts {
		info, err := directory.Lstat(strings.Join(parts[:index+1], "/"))
		if err != nil {
			return nil, err
		}
		if info.Mode()&os.ModeSymlink != 0 {
			return nil, errors.New("artifact path contains a symlink")
		}
	}
	before, err := directory.Lstat(path)
	if err != nil {
		return nil, err
	}
	file, err := directory.Open(path)
	if err != nil {
		return nil, err
	}
	after, err := file.Stat()
	if err != nil || !after.Mode().IsRegular() || !os.SameFile(before, after) {
		file.Close()
		return nil, errors.New("artifact changed while opening")
	}
	return file, nil
}
