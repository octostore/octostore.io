package octostore

import "fmt"

// OctoStoreError is the base error type for all OctoStore errors.
type OctoStoreError struct {
	Message string
}

func (e *OctoStoreError) Error() string {
	return e.Message
}

// AuthenticationError is returned when authentication fails.
type AuthenticationError struct {
	Message string
}

func (e *AuthenticationError) Error() string {
	return e.Message
}

// NetworkError is returned when network operations fail.
type NetworkError struct {
	Err error
}

func (e *NetworkError) Error() string {
	return fmt.Sprintf("network error: %v", e.Err)
}

func (e *NetworkError) Unwrap() error {
	return e.Err
}

// LockError is returned when lock operations fail.
type LockError struct {
	Message    string
	StatusCode int
}

func (e *LockError) Error() string {
	return e.Message
}

// LockHeldError is returned when trying to acquire a lock that's already held.
type LockHeldError struct {
	Message   string
	HolderID  string
	ExpiresAt string
}

func (e *LockHeldError) Error() string {
	return e.Message
}

// ValidationError is returned when input validation fails.
type ValidationError struct {
	Message string
}

func (e *ValidationError) Error() string {
	return e.Message
}