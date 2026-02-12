package octostore

// LockStatus represents the status of a lock.
type LockStatus string

const (
	// LockStatusFree indicates the lock is available.
	LockStatusFree LockStatus = "free"
	// LockStatusHeld indicates the lock is currently held.
	LockStatusHeld LockStatus = "held"
)

// LockInfo contains information about a lock.
type LockInfo struct {
	Name         string     `json:"name"`
	Status       LockStatus `json:"status"`
	HolderID     *string    `json:"holder_id"`
	FencingToken int        `json:"fencing_token"`
	ExpiresAt    *string    `json:"expires_at"`
}

// AcquireResult contains the result of a lock acquisition attempt.
type AcquireResult struct {
	Status       string  `json:"status"`
	LeaseID      *string `json:"lease_id,omitempty"`
	FencingToken *int    `json:"fencing_token,omitempty"`
	ExpiresAt    *string `json:"expires_at,omitempty"`
	HolderID     *string `json:"holder_id,omitempty"`
}

// RenewResult contains the result of a lock renewal.
type RenewResult struct {
	LeaseID   string `json:"lease_id"`
	ExpiresAt string `json:"expires_at"`
}

// TokenInfo contains information about an authentication token.
type TokenInfo struct {
	Token          string `json:"token"`
	UserID         string `json:"user_id"`
	GitHubUsername string `json:"github_username"`
}

// UserLock represents a lock owned by the user.
type UserLock struct {
	Name         string `json:"name"`
	LeaseID      string `json:"lease_id"`
	FencingToken int    `json:"fencing_token"`
	ExpiresAt    string `json:"expires_at"`
}

// AcquireRequest is the request body for acquiring a lock.
type AcquireRequest struct {
	TTLSeconds int `json:"ttl_seconds"`
}

// ReleaseRequest is the request body for releasing a lock.
type ReleaseRequest struct {
	LeaseID string `json:"lease_id"`
}

// RenewRequest is the request body for renewing a lock.
type RenewRequest struct {
	LeaseID    string `json:"lease_id"`
	TTLSeconds int    `json:"ttl_seconds"`
}

// ListLocksResponse is the response body for listing locks.
type ListLocksResponse struct {
	Locks []UserLock `json:"locks"`
}