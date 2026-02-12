# OctoStore Java SDK

Java client library for the OctoStore distributed lock service.

## Installation

### Maven

```xml
<dependency>
    <groupId>io.octostore</groupId>
    <artifactId>octostore-java-client</artifactId>
    <version>1.0.0</version>
</dependency>
```

### Gradle

```groovy
implementation 'io.octostore:octostore-java-client:1.0.0'
```

## Quick Start

```java
import io.octostore.OctoStoreClient;
import io.octostore.model.AcquireResult;
import io.octostore.exceptions.LockHeldException;

public class Example {
    public static void main(String[] args) {
        try (OctoStoreClient client = new OctoStoreClient("your-token-here")) {
            // Check health
            String health = client.health();
            System.out.println("Service status: " + health);

            // Acquire lock
            AcquireResult result = client.acquireLock("my-resource", 300);
            System.out.println("Lock acquired: " + result.getLeaseId());
            
            // Do critical work...
            
            // Release lock
            client.releaseLock("my-resource", result.getLeaseId());
            System.out.println("Lock released");
            
        } catch (LockHeldException e) {
            System.out.println("Lock held by: " + e.getHolderId());
        } catch (Exception e) {
            e.printStackTrace();
        }
    }
}
```

## Builder Pattern

```java
OctoStoreClient client = OctoStoreClient.builder()
    .baseUrl("https://api.octostore.io")
    .token("your-token")
    .timeout(Duration.ofSeconds(30))
    .build();
```

## API Reference

- `health()` - Check service health
- `acquireLock(name, ttl)` - Acquire a lock
- `releaseLock(name, leaseId)` - Release a lock
- `renewLock(name, leaseId, ttl)` - Renew a lock
- `getLockStatus(name)` - Get lock status
- `listLocks()` - List user's locks
- `rotateToken()` - Rotate auth token

## Requirements

- Java 11 or higher
- Maven 3.6+ or Gradle 6+

## License

MIT License