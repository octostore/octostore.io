package io.octostore.model;

import com.fasterxml.jackson.annotation.JsonProperty;

/**
 * Result of a lock acquisition attempt.
 */
public class AcquireResult {
    private String status;
    @JsonProperty("lease_id")
    private String leaseId;
    @JsonProperty("fencing_token")
    private Integer fencingToken;
    @JsonProperty("expires_at")
    private String expiresAt;
    @JsonProperty("holder_id")
    private String holderId;

    public AcquireResult() {}

    public String getStatus() { return status; }
    public void setStatus(String status) { this.status = status; }

    public String getLeaseId() { return leaseId; }
    public void setLeaseId(String leaseId) { this.leaseId = leaseId; }

    public Integer getFencingToken() { return fencingToken; }
    public void setFencingToken(Integer fencingToken) { this.fencingToken = fencingToken; }

    public String getExpiresAt() { return expiresAt; }
    public void setExpiresAt(String expiresAt) { this.expiresAt = expiresAt; }

    public String getHolderId() { return holderId; }
    public void setHolderId(String holderId) { this.holderId = holderId; }
}