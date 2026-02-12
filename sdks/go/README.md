# OctoStore Go SDK

Go client library for the OctoStore distributed lock service.

## Installation

```bash
go get github.com/octostore/octostore-go
```

## Quick Start

```go
package main

import (
    "context"
    "fmt"
    "log"
    "time"

    "github.com/octostore/octostore-go"
)

func main() {
    // Initialize client
    client := octostore.NewClient("https://api.octostore.io", "your-token-here")
    client.SetTimeout(30 * time.Second)

    ctx := context.Background()

    // Check service health
    health, err := client.Health(ctx)
    if err != nil {
        log.Fatalf("Health check failed: %v", err)
    }
    fmt.Printf("Service status: %s\n", health)

    // Acquire a lock
    result, err := client.AcquireLock(ctx, "my-resource", 300)
    if err != nil {
        if lockErr, ok := err.(*octostore.LockHeldError); ok {
            fmt.Printf("Lock is held by %s until %s\n", lockErr.HolderID, lockErr.ExpiresAt)
            return
        }
        log.Fatalf("Failed to acquire lock: %v", err)
    }

    fmt.Printf("Lock acquired: %s\n", *result.LeaseID)

    // Do your critical work here...

    // Release the lock
    err = client.ReleaseLock(ctx, "my-resource", *result.LeaseID)
    if err != nil {
        log.Fatalf("Failed to release lock: %v", err)
    }
    
    fmt.Println("Lock released")
}
```

## Context Support

All operations support Go's context for timeouts and cancellation:

```go
// With timeout
ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
defer cancel()

result, err := client.AcquireLock(ctx, "resource", 60)
if err != nil {
    // Handle timeout or other errors
    return
}

// With cancellation
ctx, cancel := context.WithCancel(context.Background())
go func() {
    // Cancel after some condition
    time.Sleep(5 * time.Second)
    cancel()
}()

err := client.ReleaseLock(ctx, "resource", leaseID)
```

## Advanced Usage

### Lock Renewal

```go
// Acquire lock
result, err := client.AcquireLock(ctx, "long-task", 60)
if err != nil {
    return err
}

// Renew lock periodically
go func() {
    ticker := time.NewTicker(30 * time.Second)
    defer ticker.Stop()
    
    for {
        select {
        case <-ticker.C:
            renewed, err := client.RenewLock(ctx, "long-task", *result.LeaseID, 60)
            if err != nil {
                log.Printf("Failed to renew lock: %v", err)
                return
            }
            log.Printf("Lock renewed until: %s", renewed.ExpiresAt)
        case <-ctx.Done():
            return
        }
    }
}()

// Do long-running work...

// Release when done
err = client.ReleaseLock(ctx, "long-task", *result.LeaseID)
```

### Lock Status Monitoring

```go
// Get current lock status
status, err := client.GetLockStatus(ctx, "monitored-resource")
if err != nil {
    return err
}

fmt.Printf("Lock '%s' is %s\n", status.Name, status.Status)
if status.Status == octostore.LockStatusHeld {
    fmt.Printf("Held by: %s\n", *status.HolderID)
    fmt.Printf("Expires: %s\n", *status.ExpiresAt)
    fmt.Printf("Fencing token: %d\n", status.FencingToken)
}
```

### List User Locks

```go
locks, err := client.ListLocks(ctx)
if err != nil {
    return err
}

fmt.Printf("You have %d active locks:\n", len(locks))
for _, lock := range locks {
    fmt.Printf("- %s (expires: %s)\n", lock.Name, lock.ExpiresAt)
}
```

## API Reference

### Client

#### `NewClient(baseURL, token string) *Client`
Creates a new OctoStore client.

#### `SetTimeout(timeout time.Duration)`
Sets the HTTP client timeout.

### Methods

#### `Health(ctx context.Context) (string, error)`
Check API health status.

#### `AcquireLock(ctx context.Context, name string, ttl int) (*AcquireResult, error)`
Acquire a distributed lock.
- `name` - Lock name (alphanumeric, hyphens, dots only, max 128 chars)
- `ttl` - Time-to-live in seconds (1-3600)

#### `ReleaseLock(ctx context.Context, name, leaseID string) error`
Release a distributed lock.

#### `RenewLock(ctx context.Context, name, leaseID string, ttl int) (*RenewResult, error)`
Renew a distributed lock.

#### `GetLockStatus(ctx context.Context, name string) (*LockInfo, error)`
Get the current status of a lock.

#### `ListLocks(ctx context.Context) ([]UserLock, error)`
List all locks owned by the current user.

#### `RotateToken(ctx context.Context) (*TokenInfo, error)`
Rotate the authentication token.

### Types

```go
type AcquireResult struct {
    Status       string  `json:"status"`
    LeaseID      *string `json:"lease_id,omitempty"`
    FencingToken *int    `json:"fencing_token,omitempty"`
    ExpiresAt    *string `json:"expires_at,omitempty"`
    HolderID     *string `json:"holder_id,omitempty"`
}

type LockInfo struct {
    Name         string     `json:"name"`
    Status       LockStatus `json:"status"`
    HolderID     *string    `json:"holder_id"`
    FencingToken int        `json:"fencing_token"`
    ExpiresAt    *string    `json:"expires_at"`
}

type UserLock struct {
    Name         string `json:"name"`
    LeaseID      string `json:"lease_id"`
    FencingToken int    `json:"fencing_token"`
    ExpiresAt    string `json:"expires_at"`
}
```

### Error Handling

```go
import (
    "errors"
    "github.com/octostore/octostore-go"
)

result, err := client.AcquireLock(ctx, "resource", 60)
if err != nil {
    var lockHeldErr *octostore.LockHeldError
    var authErr *octostore.AuthenticationError
    var netErr *octostore.NetworkError
    var valErr *octostore.ValidationError
    
    switch {
    case errors.As(err, &lockHeldErr):
        fmt.Printf("Lock held by: %s\n", lockHeldErr.HolderID)
    case errors.As(err, &authErr):
        fmt.Printf("Authentication failed: %s\n", authErr.Message)
    case errors.As(err, &netErr):
        fmt.Printf("Network error: %v\n", netErr.Err)
    case errors.As(err, &valErr):
        fmt.Printf("Validation error: %s\n", valErr.Message)
    default:
        fmt.Printf("Unknown error: %v\n", err)
    }
}
```

## Environment Variables

```go
import "os"

client := octostore.NewClient(
    getEnv("OCTOSTORE_BASE_URL", "https://api.octostore.io"),
    os.Getenv("OCTOSTORE_TOKEN"),
)

func getEnv(key, defaultValue string) string {
    if value := os.Getenv(key); value != "" {
        return value
    }
    return defaultValue
}
```

## Best Practices

1. **Use context** for timeouts and cancellation
2. **Handle specific error types** for better error handling
3. **Set appropriate timeouts** with `SetTimeout()`
4. **Validate inputs** before making API calls
5. **Use fencing tokens** for coordination in distributed systems
6. **Implement retry logic** with exponential backoff

## License

MIT License