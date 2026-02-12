using System.Net.Http.Json;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.RegularExpressions;

namespace OctoStore;

public class OctoStoreClient : IDisposable
{
    private readonly HttpClient _httpClient;
    private readonly string _baseUrl;
    private string? _token;
    private static readonly Regex LockNamePattern = new(@"^[a-zA-Z0-9.-]+$");

    public OctoStoreClient(string baseUrl = "https://api.octostore.io", string? token = null)
    {
        _baseUrl = baseUrl.TrimEnd('/');
        _token = token;
        _httpClient = new HttpClient();
        
        if (token is not null)
            _httpClient.DefaultRequestHeaders.Authorization = 
                new System.Net.Http.Headers.AuthenticationHeaderValue("Bearer", token);
    }

    public async Task<string> HealthAsync(CancellationToken cancellationToken = default)
    {
        var response = await _httpClient.GetAsync($"{_baseUrl}/health", cancellationToken);
        await HandleErrorsAsync(response);
        return await response.Content.ReadAsStringAsync(cancellationToken);
    }

    public async Task<AcquireResult> AcquireLockAsync(string name, int ttl = 60, CancellationToken cancellationToken = default)
    {
        ValidateLockName(name);
        ValidateTtl(ttl);

        var request = new AcquireRequest(ttl);
        var response = await _httpClient.PostAsJsonAsync($"{_baseUrl}/locks/{name}/acquire", request, cancellationToken);
        
        if (response.StatusCode == System.Net.HttpStatusCode.Conflict)
        {
            var errorData = await response.Content.ReadFromJsonAsync<ErrorResponse>(cancellationToken);
            throw new LockHeldException($"Lock '{name}' is already held", errorData?.HolderId, errorData?.ExpiresAt);
        }
        
        await HandleErrorsAsync(response);
        return await response.Content.ReadFromJsonAsync<AcquireResult>(cancellationToken) ?? 
            throw new OctoStoreException("Empty response");
    }

    public async Task ReleaseLockAsync(string name, string leaseId, CancellationToken cancellationToken = default)
    {
        ValidateLockName(name);
        var request = new ReleaseRequest(leaseId);
        var response = await _httpClient.PostAsJsonAsync($"{_baseUrl}/locks/{name}/release", request, cancellationToken);
        await HandleErrorsAsync(response);
    }

    public async Task<RenewResult> RenewLockAsync(string name, string leaseId, int ttl = 60, CancellationToken cancellationToken = default)
    {
        ValidateLockName(name);
        ValidateTtl(ttl);
        
        var request = new RenewRequest(leaseId, ttl);
        var response = await _httpClient.PostAsJsonAsync($"{_baseUrl}/locks/{name}/renew", request, cancellationToken);
        await HandleErrorsAsync(response);
        return await response.Content.ReadFromJsonAsync<RenewResult>(cancellationToken) ??
            throw new OctoStoreException("Empty response");
    }

    public async Task<LockInfo> GetLockStatusAsync(string name, CancellationToken cancellationToken = default)
    {
        ValidateLockName(name);
        var response = await _httpClient.GetAsync($"{_baseUrl}/locks/{name}", cancellationToken);
        await HandleErrorsAsync(response);
        return await response.Content.ReadFromJsonAsync<LockInfo>(cancellationToken) ??
            throw new OctoStoreException("Empty response");
    }

    public async Task<List<UserLock>> ListLocksAsync(CancellationToken cancellationToken = default)
    {
        var response = await _httpClient.GetAsync($"{_baseUrl}/locks", cancellationToken);
        await HandleErrorsAsync(response);
        var result = await response.Content.ReadFromJsonAsync<ListLocksResponse>(cancellationToken);
        return result?.Locks ?? new List<UserLock>();
    }

    public async Task<TokenInfo> RotateTokenAsync(CancellationToken cancellationToken = default)
    {
        var response = await _httpClient.PostAsync($"{_baseUrl}/auth/token/rotate", null, cancellationToken);
        await HandleErrorsAsync(response);
        var tokenInfo = await response.Content.ReadFromJsonAsync<TokenInfo>(cancellationToken) ??
            throw new OctoStoreException("Empty response");
        
        _token = tokenInfo.Token;
        _httpClient.DefaultRequestHeaders.Authorization = 
            new System.Net.Http.Headers.AuthenticationHeaderValue("Bearer", _token);
            
        return tokenInfo;
    }

    private static void ValidateLockName(string name)
    {
        if (string.IsNullOrEmpty(name) || name.Length > 128)
            throw new ValidationException("Lock name must be 1-128 characters");
        if (!LockNamePattern.IsMatch(name))
            throw new ValidationException("Lock name can only contain alphanumeric characters, hyphens, and dots");
    }

    private static void ValidateTtl(int ttl)
    {
        if (ttl < 1 || ttl > 3600)
            throw new ValidationException("TTL must be between 1 and 3600 seconds");
    }

    private static async Task HandleErrorsAsync(HttpResponseMessage response)
    {
        if (response.StatusCode == System.Net.HttpStatusCode.Unauthorized)
            throw new AuthenticationException("Invalid or missing authentication token");
        
        if (!response.IsSuccessStatusCode)
        {
            var error = await response.Content.ReadAsStringAsync();
            throw new OctoStoreException($"HTTP {(int)response.StatusCode}: {error}");
        }
    }

    public void Dispose()
    {
        _httpClient?.Dispose();
        GC.SuppressFinalize(this);
    }
}

// Models and Exceptions
public record AcquireRequest([property: JsonPropertyName("ttl_seconds")] int TtlSeconds);
public record ReleaseRequest([property: JsonPropertyName("lease_id")] string LeaseId);
public record RenewRequest([property: JsonPropertyName("lease_id")] string LeaseId, [property: JsonPropertyName("ttl_seconds")] int TtlSeconds);

public record AcquireResult(
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("lease_id")] string? LeaseId,
    [property: JsonPropertyName("fencing_token")] int? FencingToken,
    [property: JsonPropertyName("expires_at")] string? ExpiresAt,
    [property: JsonPropertyName("holder_id")] string? HolderId);

public record LockInfo(
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("status")] string Status,
    [property: JsonPropertyName("holder_id")] string? HolderId,
    [property: JsonPropertyName("fencing_token")] long FencingToken,
    [property: JsonPropertyName("expires_at")] string? ExpiresAt);

public record RenewResult(
    [property: JsonPropertyName("lease_id")] string LeaseId,
    [property: JsonPropertyName("expires_at")] string ExpiresAt);

public record TokenInfo(
    [property: JsonPropertyName("token")] string Token,
    [property: JsonPropertyName("user_id")] string UserId,
    [property: JsonPropertyName("github_username")] string GitHubUsername);

public record UserLock(
    [property: JsonPropertyName("name")] string Name,
    [property: JsonPropertyName("lease_id")] string LeaseId,
    [property: JsonPropertyName("fencing_token")] long FencingToken,
    [property: JsonPropertyName("expires_at")] string ExpiresAt);

public record ListLocksResponse([property: JsonPropertyName("locks")] List<UserLock> Locks);
public record ErrorResponse([property: JsonPropertyName("holder_id")] string? HolderId, [property: JsonPropertyName("expires_at")] string? ExpiresAt);

public class OctoStoreException : Exception
{
    public OctoStoreException(string message) : base(message) { }
    public OctoStoreException(string message, Exception innerException) : base(message, innerException) { }
}

public class AuthenticationException : OctoStoreException
{
    public AuthenticationException(string message) : base(message) { }
}

public class LockHeldException : OctoStoreException
{
    public string? HolderId { get; }
    public string? ExpiresAt { get; }
    
    public LockHeldException(string message, string? holderId, string? expiresAt) : base(message)
    {
        HolderId = holderId;
        ExpiresAt = expiresAt;
    }
}

public class ValidationException : OctoStoreException
{
    public ValidationException(string message) : base(message) { }
}