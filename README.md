# Biboop - PIN-based Temporary Data Exchange Service

Biboop is a lightweight Rust web service that provides temporary data exchange using short PIN codes. It's designed for scenarios where you need to briefly share data between systems without permanent storage.

## Features

- **Short PIN Generation**: Creates unique 4-character alphanumeric PINs
- **Namespace Support**: Organize PINs by namespace to avoid conflicts
- **Automatic Cleanup**: Removes stale PINs after 10 minutes
- **JSON Data Storage**: Store arbitrary JSON payloads up to 3KB
- **Thread-Safe**: Concurrent access with evmap for high performance
- **Health Monitoring**: Built-in health check endpoint

## Quick Start

### Prerequisites

- Rust 1.70+ (due to dependency requirements)
- Cargo package manager

### Installation & Running

```bash
# Clone the repository
git clone <repository-url>
cd configgy

# Run the service
cargo run

# Or build and run the binary
cargo build --release
./target/release/biboop
```

The service will start on `http://0.0.0.0:8080`

### Docker Deployment

```bash
# Build AMD64 Docker image
./scripts/make_amd64.sh

# The binary will be extracted to target/release/biboop-amd64
```

## API Reference

### Base URL
```
http://localhost:8080
```

### Endpoints

#### 1. Generate PIN
**POST** `/pin/{namespace}`

Generates a new unique PIN in the specified namespace.

**Example:**
```bash
curl -X POST http://localhost:8080/pin/myapp
```

**Response:**
```json
{
  "pin": "A7X9",
  "result": null
}
```

#### 2. Poll PIN
**POST** `/pin/{namespace}/{pin}`

Checks if data has been submitted to a PIN. Returns the data if available, or generates a new PIN if the current one is empty.

**Example:**
```bash
curl -X POST http://localhost:8080/pin/myapp/A7X9
```

**Response (no data yet):**
```json
{
  "pin": "B2Y4",
  "result": null
}
```

**Response (with data):**
```json
{
  "pin": "A7X9",
  "result": {
    "message": "Hello, World!",
    "timestamp": "2023-12-07T10:30:00Z"
  }
}
```

#### 3. Submit Data to PIN
**PUT** `/pin/{namespace}/{pin}`

Submits JSON data to an existing PIN.

**Example:**
```bash
curl -X PUT http://localhost:8080/pin/myapp/A7X9 \
  -H "Content-Type: application/json" \
  -d '{"message": "Hello, World!", "timestamp": "2023-12-07T10:30:00Z"}'
```

**Response:**
```
Thanks!
```

#### 4. Health Check
**GET** `/health`

Returns the service health status.

**Example:**
```bash
curl http://localhost:8080/health
```

**Response:**
```
All good.
```

## Usage Patterns

### 1. Simple Data Exchange

**Step 1: Generate a PIN**
```bash
curl -X POST http://localhost:8080/pin/chat
# Response: {"pin": "X7Z2", "result": null}
```

**Step 2: Share the PIN with recipient**
Give the PIN "X7Z2" to the person who will send data.

**Step 3: Sender submits data**
```bash
curl -X PUT http://localhost:8080/pin/chat/X7Z2 \
  -H "Content-Type: application/json" \
  -d '{"message": "Secret message", "from": "alice"}'
```

**Step 4: Receiver polls for data**
```bash
curl -X POST http://localhost:8080/pin/chat/X7Z2
# Response: {"pin": "X7Z2", "result": {"message": "Secret message", "from": "alice"}}
```

### 2. Device Pairing

**Use Case**: Pair two devices by exchanging configuration data.

```bash
# Device A generates PIN
curl -X POST http://localhost:8080/pin/pairing
# Response: {"pin": "M4K8", "result": null}

# Device A submits its config
curl -X PUT http://localhost:8080/pin/pairing/M4K8 \
  -H "Content-Type: application/json" \
  -d '{"device_id": "device-a", "ip": "192.168.1.100", "port": 8081}'

# Device B retrieves the config using the PIN
curl -X POST http://localhost:8080/pin/pairing/M4K8
# Response: {"pin": "M4K8", "result": {"device_id": "device-a", "ip": "192.168.1.100", "port": 8081}}
```

### 3. Continuous Polling

For applications that need to wait for data:

```bash
#!/bin/bash
NAMESPACE="myapp"
PIN=""

# Get initial PIN
RESPONSE=$(curl -s -X POST http://localhost:8080/pin/$NAMESPACE)
PIN=$(echo $RESPONSE | jq -r '.pin')
echo "Waiting for data on PIN: $PIN"

# Poll until data arrives
while true; do
    RESPONSE=$(curl -s -X POST http://localhost:8080/pin/$NAMESPACE/$PIN)
    RESULT=$(echo $RESPONSE | jq -r '.result')
    
    if [ "$RESULT" != "null" ]; then
        echo "Data received: $RESULT"
        break
    fi
    
    # Update PIN if service returned a new one
    NEW_PIN=$(echo $RESPONSE | jq -r '.pin')
    if [ "$NEW_PIN" != "$PIN" ]; then
        PIN=$NEW_PIN
        echo "New PIN: $PIN"
    fi
    
    sleep 2
done
```

## Configuration

### Environment Variables

The service uses dotenv for configuration. Create a `.env` file:

```env
# Logging level (error, warn, info, debug, trace)
RUST_LOG=info

# Server bind address (default: 0.0.0.0:8080)
# BIND_ADDRESS=127.0.0.1:3000
```

### Service Configuration

Key parameters (hardcoded in current version):

- **PIN Length**: 4 characters
- **Max Payload Size**: 3,000 bytes
- **PIN Expiry**: 10 minutes
- **Cleanup Interval**: 10 seconds
- **Bind Address**: 0.0.0.0:8080

## Error Handling

### Common HTTP Status Codes

- **200 OK**: Successful PIN generation or data retrieval
- **202 Accepted**: Data successfully submitted to PIN
- **404 Not Found**: PIN doesn't exist or has expired
- **413 Payload Too Large**: Submitted data exceeds 3KB limit
- **429 Too Many Requests**: Cannot generate unique PIN (try again)

### Error Responses

**PIN Not Found:**
```bash
curl -X PUT http://localhost:8080/pin/test/INVALID
# Response: "Pin not found." (404)
```

**Payload Too Large:**
```bash
curl -X PUT http://localhost:8080/pin/test/A1B2 -d '{"data": "very large payload..."}'
# Response: "Payload too large." (413)
```

## Development

### Running Tests
```bash
# Run all tests
cargo test

# Run with output
cargo test -- --nocapture

# Run specific test
cargo test test_pin_item_creation

# Run integration tests only
cargo test test_health_endpoint
```

### Code Structure

- `src/main.rs`: Main application with all endpoints and logic
- `scripts/make_amd64.sh`: Docker build script
- `Dockerfile-amd64`: Multi-stage Docker build
- `scripts/biboop.service`: Systemd service file

### Dependencies

Key dependencies and their purposes:

- `actix-web`: HTTP server framework
- `evmap`: Lock-free concurrent map for high-performance storage
- `serde`: JSON serialization/deserialization
- `chrono`: Date/time handling for expiry
- `clokwerk`: Background task scheduling
- `rand`: PIN generation

## Production Deployment

### Systemd Service

Use the provided service file:

```bash
# Copy service file
sudo cp scripts/biboop.service /etc/systemd/system/

# Enable and start
sudo systemctl enable biboop
sudo systemctl start biboop

# Check status
sudo systemctl status biboop
```

### Monitoring

Monitor the service health:

```bash
# Health check
curl http://localhost:8080/health

# Check logs
sudo journalctl -u biboop -f
```

### Security Considerations

- No authentication mechanism - deploy behind a proxy with auth if needed
- PINs are short and may be guessable - use appropriate namespacing
- Data is stored in memory only - lost on restart
- No rate limiting - consider adding reverse proxy with rate limiting

## Limitations

- **Memory Only**: All data is lost on service restart
- **No Persistence**: PINs and data are not saved to disk
- **No Authentication**: Anyone can access any PIN if they guess it
- **No Rate Limiting**: No built-in protection against abuse
- **Fixed Configuration**: Key parameters are hardcoded

## Recent Improvements

✅ **Dependencies Updated**: All dependencies updated to modern, compatible versions  
✅ **Tests Working**: Comprehensive test suite now passes  
✅ **Actix-web 4.x**: Upgraded to latest framework version  
✅ **Modern Rust**: Compatible with current Rust toolchain

## License

[Add your license information here]

## Contributing

[Add contribution guidelines here]