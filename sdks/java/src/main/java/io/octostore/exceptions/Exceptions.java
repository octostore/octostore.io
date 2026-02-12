package io.octostore.exceptions;

/**
 * Base exception for all OctoStore errors.
 */
class OctoStoreException extends Exception {
    public OctoStoreException(String message) {
        super(message);
    }
    
    public OctoStoreException(String message, Throwable cause) {
        super(message, cause);
    }
}

/**
 * Authentication failed exception.
 */
class AuthenticationException extends OctoStoreException {
    public AuthenticationException(String message) {
        super(message);
    }
}

/**
 * Network error exception.
 */
class NetworkException extends OctoStoreException {
    public NetworkException(String message, Throwable cause) {
        super(message, cause);
    }
}

/**
 * Lock operation failed exception.
 */
class LockException extends OctoStoreException {
    private final int statusCode;
    
    public LockException(String message, int statusCode) {
        super(message);
        this.statusCode = statusCode;
    }
    
    public int getStatusCode() {
        return statusCode;
    }
}

/**
 * Lock is currently held exception.
 */
class LockHeldException extends OctoStoreException {
    private final String holderId;
    private final String expiresAt;
    
    public LockHeldException(String message, String holderId, String expiresAt) {
        super(message);
        this.holderId = holderId;
        this.expiresAt = expiresAt;
    }
    
    public String getHolderId() {
        return holderId;
    }
    
    public String getExpiresAt() {
        return expiresAt;
    }
}

/**
 * Input validation failed exception.
 */
class ValidationException extends RuntimeException {
    public ValidationException(String message) {
        super(message);
    }
}