// Package octostore provides a Go client for the OctoStore distributed lock service.
package octostore

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"regexp"
	"strings"
	"time"
)

// Client represents an OctoStore client.
type Client struct {
	baseURL    string
	token      string
	httpClient *http.Client
}

// NewClient creates a new OctoStore client.
func NewClient(baseURL, token string) *Client {
	return &Client{
		baseURL: strings.TrimSuffix(baseURL, "/"),
		token:   token,
		httpClient: &http.Client{
			Timeout: 30 * time.Second,
		},
	}
}

// SetTimeout sets the HTTP client timeout.
func (c *Client) SetTimeout(timeout time.Duration) {
	c.httpClient.Timeout = timeout
}

// doRequest performs an HTTP request with proper error handling.
func (c *Client) doRequest(ctx context.Context, method, path string, body interface{}) (*http.Response, error) {
	var reqBody io.Reader
	
	if body != nil {
		jsonBody, err := json.Marshal(body)
		if err != nil {
			return nil, fmt.Errorf("failed to marshal request body: %w", err)
		}
		reqBody = bytes.NewReader(jsonBody)
	}
	
	req, err := http.NewRequestWithContext(ctx, method, c.baseURL+path, reqBody)
	if err != nil {
		return nil, fmt.Errorf("failed to create request: %w", err)
	}
	
	req.Header.Set("Content-Type", "application/json")
	if c.token != "" {
		req.Header.Set("Authorization", "Bearer "+c.token)
	}
	
	resp, err := c.httpClient.Do(req)
	if err != nil {
		return nil, &NetworkError{Err: err}
	}
	
	return resp, nil
}

// handleResponse processes the HTTP response and handles errors.
func (c *Client) handleResponse(resp *http.Response, result interface{}) error {
	defer resp.Body.Close()
	
	body, err := io.ReadAll(resp.Body)
	if err != nil {
		return &NetworkError{Err: fmt.Errorf("failed to read response body: %w", err)}
	}
	
	switch resp.StatusCode {
	case http.StatusUnauthorized:
		return &AuthenticationError{Message: "Invalid or missing authentication token"}
	case http.StatusConflict:
		if strings.Contains(string(body), "acquire") {
			var errorResp map[string]interface{}
			if err := json.Unmarshal(body, &errorResp); err == nil {
				holderID, _ := errorResp["holder_id"].(string)
				expiresAt, _ := errorResp["expires_at"].(string)
				return &LockHeldError{
					Message:   "Lock is already held",
					HolderID:  holderID,
					ExpiresAt: expiresAt,
				}
			}
		}
		fallthrough
	default:
		if resp.StatusCode >= 400 {
			var errorResp map[string]interface{}
			var message string
			if err := json.Unmarshal(body, &errorResp); err == nil {
				if errMsg, ok := errorResp["error"].(string); ok {
					message = errMsg
				}
			}
			if message == "" {
				message = fmt.Sprintf("HTTP %d: %s", resp.StatusCode, string(body))
			}
			return &LockError{Message: message, StatusCode: resp.StatusCode}
		}
	}
	
	// Handle empty responses
	if len(body) == 0 {
		return nil
	}
	
	// Handle plain text responses
	if strings.Contains(resp.Header.Get("Content-Type"), "application/json") {
		if result != nil {
			if err := json.Unmarshal(body, result); err != nil {
				return &NetworkError{Err: fmt.Errorf("failed to unmarshal response: %w", err)}
			}
		}
	} else {
		// For health check and other plain text responses
		if strResult, ok := result.(*string); ok {
			*strResult = strings.Trim(string(body), "\"")
		}
	}
	
	return nil
}

// validateLockName validates the lock name format.
func validateLockName(name string) error {
	if len(name) == 0 || len(name) > 128 {
		return &ValidationError{Message: "Lock name must be 1-128 characters"}
	}
	
	matched, err := regexp.MatchString(`^[a-zA-Z0-9.-]+$`, name)
	if err != nil {
		return &ValidationError{Message: "Failed to validate lock name"}
	}
	if !matched {
		return &ValidationError{Message: "Lock name can only contain alphanumeric characters, hyphens, and dots"}
	}
	
	return nil
}

// validateTTL validates the TTL value.
func validateTTL(ttl int) error {
	if ttl < 1 || ttl > 3600 {
		return &ValidationError{Message: "TTL must be between 1 and 3600 seconds"}
	}
	return nil
}

// Health checks the API health status.
func (c *Client) Health(ctx context.Context) (string, error) {
	resp, err := c.doRequest(ctx, "GET", "/health", nil)
	if err != nil {
		return "", err
	}
	
	var result string
	if err := c.handleResponse(resp, &result); err != nil {
		return "", err
	}
	
	return result, nil
}

// AcquireLock attempts to acquire a distributed lock.
func (c *Client) AcquireLock(ctx context.Context, name string, ttl int) (*AcquireResult, error) {
	if err := validateLockName(name); err != nil {
		return nil, err
	}
	if err := validateTTL(ttl); err != nil {
		return nil, err
	}
	
	reqBody := AcquireRequest{TTLSeconds: ttl}
	
	resp, err := c.doRequest(ctx, "POST", fmt.Sprintf("/locks/%s/acquire", name), reqBody)
	if err != nil {
		return nil, err
	}
	
	var result AcquireResult
	if err := c.handleResponse(resp, &result); err != nil {
		return nil, err
	}
	
	return &result, nil
}

// ReleaseLock releases a distributed lock.
func (c *Client) ReleaseLock(ctx context.Context, name, leaseID string) error {
	if err := validateLockName(name); err != nil {
		return err
	}
	
	reqBody := ReleaseRequest{LeaseID: leaseID}
	
	resp, err := c.doRequest(ctx, "POST", fmt.Sprintf("/locks/%s/release", name), reqBody)
	if err != nil {
		return err
	}
	
	return c.handleResponse(resp, nil)
}

// RenewLock renews a distributed lock.
func (c *Client) RenewLock(ctx context.Context, name, leaseID string, ttl int) (*RenewResult, error) {
	if err := validateLockName(name); err != nil {
		return nil, err
	}
	if err := validateTTL(ttl); err != nil {
		return nil, err
	}
	
	reqBody := RenewRequest{
		LeaseID:    leaseID,
		TTLSeconds: ttl,
	}
	
	resp, err := c.doRequest(ctx, "POST", fmt.Sprintf("/locks/%s/renew", name), reqBody)
	if err != nil {
		return nil, err
	}
	
	var result RenewResult
	if err := c.handleResponse(resp, &result); err != nil {
		return nil, err
	}
	
	return &result, nil
}

// GetLockStatus gets the current status of a lock.
func (c *Client) GetLockStatus(ctx context.Context, name string) (*LockInfo, error) {
	if err := validateLockName(name); err != nil {
		return nil, err
	}
	
	resp, err := c.doRequest(ctx, "GET", fmt.Sprintf("/locks/%s", name), nil)
	if err != nil {
		return nil, err
	}
	
	var result LockInfo
	if err := c.handleResponse(resp, &result); err != nil {
		return nil, err
	}
	
	return &result, nil
}

// ListLocks lists all locks owned by the current user.
func (c *Client) ListLocks(ctx context.Context) ([]UserLock, error) {
	resp, err := c.doRequest(ctx, "GET", "/locks", nil)
	if err != nil {
		return nil, err
	}
	
	var result ListLocksResponse
	if err := c.handleResponse(resp, &result); err != nil {
		return nil, err
	}
	
	return result.Locks, nil
}

// RotateToken rotates the authentication token.
func (c *Client) RotateToken(ctx context.Context) (*TokenInfo, error) {
	resp, err := c.doRequest(ctx, "POST", "/auth/token/rotate", nil)
	if err != nil {
		return nil, err
	}
	
	var result TokenInfo
	if err := c.handleResponse(resp, &result); err != nil {
		return nil, err
	}
	
	// Update the client's token
	c.token = result.Token
	
	return &result, nil
}