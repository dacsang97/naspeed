package greet

import (
	"testing"

	"example.test/svc/internal/testutil"
)

func TestText(t *testing.T) {
	if !testutil.NonEmpty(Text()) {
		t.Fatal("greeting is empty")
	}
}
