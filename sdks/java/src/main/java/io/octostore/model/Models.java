package io.octostore.model;

import com.fasterxml.jackson.annotation.JsonProperty;
import java.util.List;

class LockInfo {
    private String name;
    private LockStatus status;
    @JsonProperty("holder_id")
    private String holderId;
    @JsonProperty("fencing_token")
    private long fencingToken;
    @JsonProperty("expires_at")
    private String expiresAt;

    // Getters and setters
    public String getName() { return name; }
    public void setName(String name) { this.name = name; }
    public LockStatus getStatus() { return status; }
    public void setStatus(LockStatus status) { this.status = status; }
    public String getHolderId() { return holderId; }
    public void setHolderId(String holderId) { this.holderId = holderId; }
    public long getFencingToken() { return fencingToken; }
    public void setFencingToken(long fencingToken) { this.fencingToken = fencingToken; }
    public String getExpiresAt() { return expiresAt; }
    public void setExpiresAt(String expiresAt) { this.expiresAt = expiresAt; }
}

class RenewResult {
    @JsonProperty("lease_id")
    private String leaseId;
    @JsonProperty("expires_at")
    private String expiresAt;

    public RenewResult() {}
    public String getLeaseId() { return leaseId; }
    public void setLeaseId(String leaseId) { this.leaseId = leaseId; }
    public String getExpiresAt() { return expiresAt; }
    public void setExpiresAt(String expiresAt) { this.expiresAt = expiresAt; }
}

class TokenInfo {
    private String token;
    @JsonProperty("user_id")
    private String userId;
    @JsonProperty("github_username")
    private String githubUsername;

    public TokenInfo() {}
    public String getToken() { return token; }
    public void setToken(String token) { this.token = token; }
    public String getUserId() { return userId; }
    public void setUserId(String userId) { this.userId = userId; }
    public String getGithubUsername() { return githubUsername; }
    public void setGithubUsername(String githubUsername) { this.githubUsername = githubUsername; }
}

class UserLock {
    private String name;
    @JsonProperty("lease_id")
    private String leaseId;
    @JsonProperty("fencing_token")
    private long fencingToken;
    @JsonProperty("expires_at")
    private String expiresAt;

    public UserLock() {}
    public String getName() { return name; }
    public void setName(String name) { this.name = name; }
    public String getLeaseId() { return leaseId; }
    public void setLeaseId(String leaseId) { this.leaseId = leaseId; }
    public long getFencingToken() { return fencingToken; }
    public void setFencingToken(long fencingToken) { this.fencingToken = fencingToken; }
    public String getExpiresAt() { return expiresAt; }
    public void setExpiresAt(String expiresAt) { this.expiresAt = expiresAt; }
}

// Request classes
class AcquireRequest {
    @JsonProperty("ttl_seconds")
    private int ttlSeconds;
    
    public AcquireRequest() {}
    public AcquireRequest(int ttlSeconds) { this.ttlSeconds = ttlSeconds; }
    public int getTtlSeconds() { return ttlSeconds; }
    public void setTtlSeconds(int ttlSeconds) { this.ttlSeconds = ttlSeconds; }
}

class ReleaseRequest {
    @JsonProperty("lease_id")
    private String leaseId;
    
    public ReleaseRequest() {}
    public ReleaseRequest(String leaseId) { this.leaseId = leaseId; }
    public String getLeaseId() { return leaseId; }
    public void setLeaseId(String leaseId) { this.leaseId = leaseId; }
}

class RenewRequest {
    @JsonProperty("lease_id")
    private String leaseId;
    @JsonProperty("ttl_seconds")
    private int ttlSeconds;
    
    public RenewRequest() {}
    public RenewRequest(String leaseId, int ttlSeconds) {
        this.leaseId = leaseId;
        this.ttlSeconds = ttlSeconds;
    }
    public String getLeaseId() { return leaseId; }
    public void setLeaseId(String leaseId) { this.leaseId = leaseId; }
    public int getTtlSeconds() { return ttlSeconds; }
    public void setTtlSeconds(int ttlSeconds) { this.ttlSeconds = ttlSeconds; }
}

class ListLocksResponse {
    private List<UserLock> locks;
    
    public ListLocksResponse() {}
    public List<UserLock> getLocks() { return locks; }
    public void setLocks(List<UserLock> locks) { this.locks = locks; }
}

class ErrorResponse {
    private String error;
    @JsonProperty("holder_id")
    private String holderId;
    @JsonProperty("expires_at")
    private String expiresAt;
    
    public ErrorResponse() {}
    public String getError() { return error; }
    public void setError(String error) { this.error = error; }
    public String getHolderId() { return holderId; }
    public void setHolderId(String holderId) { this.holderId = holderId; }
    public String getExpiresAt() { return expiresAt; }
    public void setExpiresAt(String expiresAt) { this.expiresAt = expiresAt; }
}