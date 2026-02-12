package io.octostore;

import com.fasterxml.jackson.annotation.JsonInclude;
import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.databind.PropertyNamingStrategies;
import io.octostore.exceptions.*;
import io.octostore.model.*;
import okhttp3.*;

import java.io.IOException;
import java.time.Duration;
import java.util.List;
import java.util.regex.Pattern;

/**
 * OctoStore distributed lock service client.
 */
public class OctoStoreClient implements AutoCloseable {
    private static final String DEFAULT_BASE_URL = "https://api.octostore.io";
    private static final Duration DEFAULT_TIMEOUT = Duration.ofSeconds(30);
    private static final Pattern LOCK_NAME_PATTERN = Pattern.compile("^[a-zA-Z0-9.-]+$");
    
    private final String baseUrl;
    private String token;
    private final OkHttpClient httpClient;
    private final ObjectMapper objectMapper;

    /**
     * Create a new OctoStore client with default settings.
     *
     * @param token Bearer token for authentication
     */
    public OctoStoreClient(String token) {
        this(DEFAULT_BASE_URL, token);
    }

    /**
     * Create a new OctoStore client.
     *
     * @param baseUrl Base URL of the OctoStore API
     * @param token Bearer token for authentication
     */
    public OctoStoreClient(String baseUrl, String token) {
        this(baseUrl, token, DEFAULT_TIMEOUT);
    }

    /**
     * Create a new OctoStore client with custom timeout.
     *
     * @param baseUrl Base URL of the OctoStore API
     * @param token Bearer token for authentication
     * @param timeout Request timeout
     */
    public OctoStoreClient(String baseUrl, String token, Duration timeout) {
        this.baseUrl = baseUrl.replaceAll("/$", "");
        this.token = token;
        
        this.httpClient = new OkHttpClient.Builder()
                .connectTimeout(timeout)
                .readTimeout(timeout)
                .writeTimeout(timeout)
                .build();
        
        this.objectMapper = new ObjectMapper()
                .setPropertyNamingStrategy(PropertyNamingStrategies.SNAKE_CASE)
                .setSerializationInclusion(JsonInclude.Include.NON_NULL);
    }

    /**
     * Create a builder for configuring the client.
     *
     * @return A new builder instance
     */
    public static Builder builder() {
        return new Builder();
    }

    /**
     * Builder for OctoStoreClient configuration.
     */
    public static class Builder {
        private String baseUrl = DEFAULT_BASE_URL;
        private String token;
        private Duration timeout = DEFAULT_TIMEOUT;

        public Builder baseUrl(String baseUrl) {
            this.baseUrl = baseUrl;
            return this;
        }

        public Builder token(String token) {
            this.token = token;
            return this;
        }

        public Builder timeout(Duration timeout) {
            this.timeout = timeout;
            return this;
        }

        public OctoStoreClient build() {
            return new OctoStoreClient(baseUrl, token, timeout);
        }
    }

    private void validateLockName(String name) {
        if (name == null || name.isEmpty() || name.length() > 128) {
            throw new ValidationException("Lock name must be 1-128 characters");
        }
        if (!LOCK_NAME_PATTERN.matcher(name).matches()) {
            throw new ValidationException("Lock name can only contain alphanumeric characters, hyphens, and dots");
        }
    }

    private void validateTtl(int ttl) {
        if (ttl < 1 || ttl > 3600) {
            throw new ValidationException("TTL must be between 1 and 3600 seconds");
        }
    }

    private <T> T executeRequest(String method, String path, Object requestBody, Class<T> responseType) 
            throws IOException, OctoStoreException {
        
        String url = baseUrl + path;
        Request.Builder requestBuilder = new Request.Builder().url(url);
        
        if (token != null) {
            requestBuilder.addHeader("Authorization", "Bearer " + token);
        }
        
        RequestBody body = null;
        if (requestBody != null) {
            String json = objectMapper.writeValueAsString(requestBody);
            body = RequestBody.create(json, MediaType.get("application/json"));
        }
        
        Request request = requestBuilder.method(method, body).build();
        
        try (Response response = httpClient.newCall(request).execute()) {
            return handleResponse(response, responseType);
        }
    }

    private <T> T handleResponse(Response response, Class<T> responseType) throws IOException, OctoStoreException {
        int statusCode = response.code();
        
        if (statusCode == 401) {
            throw new AuthenticationException("Invalid or missing authentication token");
        }
        
        if (statusCode == 409) {
            try {
                ErrorResponse errorResponse = objectMapper.readValue(response.body().string(), ErrorResponse.class);
                throw new LockHeldException("Lock is already held", 
                    errorResponse.getHolderId(), 
                    errorResponse.getExpiresAt());
            } catch (IOException e) {
                throw new LockHeldException("Lock is already held", null, null);
            }
        }
        
        if (statusCode >= 400) {
            String errorMessage;
            try {
                ErrorResponse errorResponse = objectMapper.readValue(response.body().string(), ErrorResponse.class);
                errorMessage = errorResponse.getError();
            } catch (IOException e) {
                errorMessage = "HTTP " + statusCode;
            }
            throw new LockException(errorMessage, statusCode);
        }
        
        String responseBody = response.body().string();
        
        if (responseBody.isEmpty()) {
            return null;
        }
        
        // Handle plain text responses (like health check)
        if (responseType == String.class && !responseBody.startsWith("{")) {
            return responseType.cast(responseBody.replaceAll("\"", ""));
        }
        
        return objectMapper.readValue(responseBody, responseType);
    }

    /**
     * Check API health status.
     *
     * @return Health status string
     * @throws OctoStoreException if the request fails
     */
    public String health() throws OctoStoreException {
        try {
            return executeRequest("GET", "/health", null, String.class);
        } catch (IOException e) {
            throw new NetworkException("Network error during health check", e);
        }
    }

    /**
     * Acquire a distributed lock.
     *
     * @param name Lock name (alphanumeric, hyphens, dots only, max 128 chars)
     * @param ttl Time-to-live in seconds (1-3600)
     * @return Acquisition result
     * @throws OctoStoreException if the request fails
     */
    public AcquireResult acquireLock(String name, int ttl) throws OctoStoreException {
        validateLockName(name);
        validateTtl(ttl);
        
        try {
            AcquireRequest request = new AcquireRequest(ttl);
            return executeRequest("POST", "/locks/" + name + "/acquire", request, AcquireResult.class);
        } catch (IOException e) {
            throw new NetworkException("Network error acquiring lock", e);
        }
    }

    /**
     * Acquire a lock with default TTL of 60 seconds.
     *
     * @param name Lock name
     * @return Acquisition result
     * @throws OctoStoreException if the request fails
     */
    public AcquireResult acquireLock(String name) throws OctoStoreException {
        return acquireLock(name, 60);
    }

    /**
     * Release a distributed lock.
     *
     * @param name Lock name
     * @param leaseId Lease ID from acquire operation
     * @throws OctoStoreException if the request fails
     */
    public void releaseLock(String name, String leaseId) throws OctoStoreException {
        validateLockName(name);
        
        try {
            ReleaseRequest request = new ReleaseRequest(leaseId);
            executeRequest("POST", "/locks/" + name + "/release", request, Void.class);
        } catch (IOException e) {
            throw new NetworkException("Network error releasing lock", e);
        }
    }

    /**
     * Renew a distributed lock.
     *
     * @param name Lock name
     * @param leaseId Lease ID from acquire operation
     * @param ttl New time-to-live in seconds (1-3600)
     * @return Renewal result
     * @throws OctoStoreException if the request fails
     */
    public RenewResult renewLock(String name, String leaseId, int ttl) throws OctoStoreException {
        validateLockName(name);
        validateTtl(ttl);
        
        try {
            RenewRequest request = new RenewRequest(leaseId, ttl);
            return executeRequest("POST", "/locks/" + name + "/renew", request, RenewResult.class);
        } catch (IOException e) {
            throw new NetworkException("Network error renewing lock", e);
        }
    }

    /**
     * Renew a lock with default TTL of 60 seconds.
     *
     * @param name Lock name
     * @param leaseId Lease ID from acquire operation
     * @return Renewal result
     * @throws OctoStoreException if the request fails
     */
    public RenewResult renewLock(String name, String leaseId) throws OctoStoreException {
        return renewLock(name, leaseId, 60);
    }

    /**
     * Get the current status of a lock.
     *
     * @param name Lock name
     * @return Lock information
     * @throws OctoStoreException if the request fails
     */
    public LockInfo getLockStatus(String name) throws OctoStoreException {
        validateLockName(name);
        
        try {
            return executeRequest("GET", "/locks/" + name, null, LockInfo.class);
        } catch (IOException e) {
            throw new NetworkException("Network error getting lock status", e);
        }
    }

    /**
     * List all locks owned by the current user.
     *
     * @return List of user locks
     * @throws OctoStoreException if the request fails
     */
    public List<UserLock> listLocks() throws OctoStoreException {
        try {
            ListLocksResponse response = executeRequest("GET", "/locks", null, ListLocksResponse.class);
            return response.getLocks();
        } catch (IOException e) {
            throw new NetworkException("Network error listing locks", e);
        }
    }

    /**
     * Rotate the authentication token.
     *
     * @return New token information
     * @throws OctoStoreException if the request fails
     */
    public TokenInfo rotateToken() throws OctoStoreException {
        try {
            TokenInfo tokenInfo = executeRequest("POST", "/auth/token/rotate", null, TokenInfo.class);
            // Update the client's token
            this.token = tokenInfo.getToken();
            return tokenInfo;
        } catch (IOException e) {
            throw new NetworkException("Network error rotating token", e);
        }
    }

    /**
     * Get the current authentication token.
     *
     * @return Current token or null if not set
     */
    public String getToken() {
        return token;
    }

    /**
     * Set the authentication token.
     *
     * @param token New token
     */
    public void setToken(String token) {
        this.token = token;
    }

    @Override
    public void close() {
        httpClient.dispatcher().executorService().shutdown();
        httpClient.connectionPool().evictAll();
    }
}