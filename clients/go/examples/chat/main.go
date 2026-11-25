// Command chat sends one chat call and one streamed call and prints the
// answers:
//
//	GATEWAY_URL=https://gateway.example.com GATEWAY_API_KEY=gw_... go run ./examples/chat chat-default "Say hello."
package main

import (
	"context"
	"fmt"
	"log"
	"os"

	gateway "github.com/qucelerate/ai-gateway/clients/go"
)

func main() {
	model, prompt := "chat-default", "Say hello."
	if len(os.Args) > 1 {
		model = os.Args[1]
	}
	if len(os.Args) > 2 {
		prompt = os.Args[2]
	}
	client, err := gateway.NewClient(os.Getenv("GATEWAY_URL"), os.Getenv("GATEWAY_API_KEY"))
	if err != nil {
		log.Fatal(err)
	}
	ctx := context.Background()
	request := gateway.ChatRequest{
		Model:     model,
		Messages:  []gateway.Message{{Role: gateway.RoleUser, Content: prompt}},
		MaxTokens: gateway.Int(256),
	}

	response, err := client.Chat(ctx, request)
	if err != nil {
		log.Fatal(err)
	}
	fmt.Println("chat:", response.Content())
	if response.Usage != nil && response.Usage.OutputTokens != nil {
		fmt.Println("output tokens:", *response.Usage.OutputTokens)
	}

	stream, err := client.ChatStream(ctx, request)
	if err != nil {
		log.Fatal(err)
	}
	defer stream.Close()
	fmt.Print("stream: ")
	for stream.Next() {
		switch event := stream.Event().(type) {
		case gateway.ChoiceDelta:
			if event.ContentDelta != nil {
				fmt.Print(*event.ContentDelta)
			}
		case gateway.ResponseCompleted:
			fmt.Println()
		}
	}
	if err := stream.Err(); err != nil {
		log.Fatal(err)
	}
	fmt.Println("request id:", stream.RequestID)
}
