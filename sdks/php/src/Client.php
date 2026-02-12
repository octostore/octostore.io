<?php

namespace OctoStore;

use GuzzleHttp\Client as HttpClient;
use GuzzleHttp\Exception\RequestException;
use GuzzleHttp\Exception\ClientException;

class Client
{
    private string $baseUrl;
    private ?string $token;
    private HttpClient $httpClient;

    public function __construct(string $baseUrl = 'https://api.octostore.io', ?string $token = null, int $timeout = 30)
    {
        $this->baseUrl = rtrim($baseUrl, '/');
        $this->token = $token;
        
        $this->httpClient = new HttpClient([
            'timeout' => $timeout,
            'headers' => [
                'Content-Type' => 'application/json',
                'Authorization' => $token ? "Bearer {$token}" : null,
            ],
        ]);
    }

    public function health(): string
    {
        try {
            $response = $this->httpClient->get("{$this->baseUrl}/health");
            return trim($response->getBody()->getContents(), '"');
        } catch (RequestException $e) {
            throw new NetworkException("Network error during health check: " . $e->getMessage(), 0, $e);
        }
    }

    public function acquireLock(string $name, int $ttl = 60): array
    {
        $this->validateLockName($name);
        $this->validateTtl($ttl);

        try {
            $response = $this->httpClient->post("{$this->baseUrl}/locks/{$name}/acquire", [
                'json' => ['ttl_seconds' => $ttl]
            ]);
            
            return json_decode($response->getBody()->getContents(), true);
            
        } catch (ClientException $e) {
            if ($e->getResponse()->getStatusCode() === 409) {
                $data = json_decode($e->getResponse()->getBody()->getContents(), true);
                throw new LockHeldException("Lock '{$name}' is already held", $data['holder_id'] ?? null, $data['expires_at'] ?? null);
            }
            $this->handleClientError($e);
        } catch (RequestException $e) {
            throw new NetworkException("Network error acquiring lock: " . $e->getMessage(), 0, $e);
        }
    }

    public function releaseLock(string $name, string $leaseId): void
    {
        $this->validateLockName($name);

        try {
            $this->httpClient->post("{$this->baseUrl}/locks/{$name}/release", [
                'json' => ['lease_id' => $leaseId]
            ]);
        } catch (ClientException $e) {
            $this->handleClientError($e);
        } catch (RequestException $e) {
            throw new NetworkException("Network error releasing lock: " . $e->getMessage(), 0, $e);
        }
    }

    public function renewLock(string $name, string $leaseId, int $ttl = 60): array
    {
        $this->validateLockName($name);
        $this->validateTtl($ttl);

        try {
            $response = $this->httpClient->post("{$this->baseUrl}/locks/{$name}/renew", [
                'json' => ['lease_id' => $leaseId, 'ttl_seconds' => $ttl]
            ]);
            
            return json_decode($response->getBody()->getContents(), true);
            
        } catch (ClientException $e) {
            $this->handleClientError($e);
        } catch (RequestException $e) {
            throw new NetworkException("Network error renewing lock: " . $e->getMessage(), 0, $e);
        }
    }

    public function getLockStatus(string $name): array
    {
        $this->validateLockName($name);

        try {
            $response = $this->httpClient->get("{$this->baseUrl}/locks/{$name}");
            return json_decode($response->getBody()->getContents(), true);
        } catch (ClientException $e) {
            $this->handleClientError($e);
        } catch (RequestException $e) {
            throw new NetworkException("Network error getting lock status: " . $e->getMessage(), 0, $e);
        }
    }

    public function listLocks(): array
    {
        try {
            $response = $this->httpClient->get("{$this->baseUrl}/locks");
            $result = json_decode($response->getBody()->getContents(), true);
            return $result['locks'] ?? [];
        } catch (ClientException $e) {
            $this->handleClientError($e);
        } catch (RequestException $e) {
            throw new NetworkException("Network error listing locks: " . $e->getMessage(), 0, $e);
        }
    }

    public function rotateToken(): array
    {
        try {
            $response = $this->httpClient->post("{$this->baseUrl}/auth/token/rotate");
            $tokenInfo = json_decode($response->getBody()->getContents(), true);
            
            $this->token = $tokenInfo['token'];
            $this->httpClient = new HttpClient([
                'timeout' => 30,
                'headers' => [
                    'Content-Type' => 'application/json',
                    'Authorization' => "Bearer {$this->token}",
                ],
            ]);
            
            return $tokenInfo;
        } catch (ClientException $e) {
            $this->handleClientError($e);
        } catch (RequestException $e) {
            throw new NetworkException("Network error rotating token: " . $e->getMessage(), 0, $e);
        }
    }

    private function validateLockName(string $name): void
    {
        if (empty($name) || strlen($name) > 128) {
            throw new ValidationException("Lock name must be 1-128 characters");
        }
        if (!preg_match('/^[a-zA-Z0-9.-]+$/', $name)) {
            throw new ValidationException("Lock name can only contain alphanumeric characters, hyphens, and dots");
        }
    }

    private function validateTtl(int $ttl): void
    {
        if ($ttl < 1 || $ttl > 3600) {
            throw new ValidationException("TTL must be between 1 and 3600 seconds");
        }
    }

    private function handleClientError(ClientException $e): void
    {
        $statusCode = $e->getResponse()->getStatusCode();
        
        if ($statusCode === 401) {
            throw new AuthenticationException("Invalid or missing authentication token");
        }
        
        $errorData = json_decode($e->getResponse()->getBody()->getContents(), true);
        $message = $errorData['error'] ?? "HTTP {$statusCode}";
        throw new LockException($message, $statusCode);
    }

    public function getToken(): ?string
    {
        return $this->token;
    }

    public function setToken(string $token): void
    {
        $this->token = $token;
        $this->httpClient = new HttpClient([
            'timeout' => 30,
            'headers' => [
                'Content-Type' => 'application/json',
                'Authorization' => "Bearer {$token}",
            ],
        ]);
    }
}

// Exception classes
class OctoStoreException extends \Exception {}
class AuthenticationException extends OctoStoreException {}
class NetworkException extends OctoStoreException {}
class LockException extends OctoStoreException {
    public int $statusCode;
    
    public function __construct(string $message, int $statusCode, \Throwable $previous = null) {
        parent::__construct($message, 0, $previous);
        $this->statusCode = $statusCode;
    }
}

class LockHeldException extends LockException {
    public ?string $holderId;
    public ?string $expiresAt;
    
    public function __construct(string $message, ?string $holderId = null, ?string $expiresAt = null) {
        parent::__construct($message, 409);
        $this->holderId = $holderId;
        $this->expiresAt = $expiresAt;
    }
}

class ValidationException extends OctoStoreException {}