// Installed-module smoke test: the released module builds as a dependency,
// and a call reaches its HTTP stack (a closed port: a transport error).
package main

import (
	"context"
	"errors"
	"fmt"
	"net/url"
	"os"

	gateway "github.com/qucelerate/ai-gateway/clients/go"
)

func main() {
	client, err := gateway.NewClient("http://127.0.0.1:1", "gw_smoke")
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	request := gateway.ChatRequest{Model: "smoke", Messages: []gateway.Message{{Role: gateway.RoleUser, Content: "hi"}}}
	_, err = client.Chat(context.Background(), request)
	var transport *url.Error
	if !errors.As(err, &transport) {
		fmt.Fprintf(os.Stderr, "expected a transport error, got %v\n", err)
		os.Exit(1)
	}
	fmt.Printf("ok: go client %s installs and runs\n", gateway.Version)
}
