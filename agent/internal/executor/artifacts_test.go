package executor

import (
	"os"
	"path/filepath"
	"testing"
)

func TestCollectRejectsEscapesAndSymlinks(test *testing.T) {
	root := test.TempDir()
	if err := os.Mkdir(filepath.Join(root, "dist"), 0700); err != nil {
		test.Fatal(err)
	}
	if err := os.WriteFile(filepath.Join(root, "dist", "app"), []byte("hello"), 0600); err != nil {
		test.Fatal(err)
	}
	files, err := Collect(root, []string{"dist/*"})
	if err != nil || len(files) != 1 {
		test.Fatalf("collect: %v %v", files, err)
	}
	for _, pattern := range []string{"../*", "/etc/*", "C:\\private\\*", "dist/../*"} {
		if _, err := Collect(root, []string{pattern}); err == nil {
			test.Fatalf("accepted %q", pattern)
		}
	}
	if err := os.Symlink(filepath.Join(root, "dist", "app"), filepath.Join(root, "dist", "link")); err == nil {
		if _, err = Collect(root, []string{"dist/*"}); err == nil {
			test.Fatal("accepted symlink")
		}
	}
	if _, err := Collect(root, []string{"missing/*"}); err == nil {
		test.Fatal("accepted missing required artifacts")
	}
}
