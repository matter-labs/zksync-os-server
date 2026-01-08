#!/bin/bash

set -e

# Color codes for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Array to store PIDs of background processes
declare -a PIDS=()

# Cleanup function to stop all started services
cleanup() {
    echo -e "\n${YELLOW}Shutting down all services...${NC}"
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            echo -e "${YELLOW}Stopping process $pid${NC}"
            kill -TERM "$pid" 2>/dev/null || true
        fi
    done
    
    # Wait for processes to terminate gracefully
    sleep 2
    
    # Force kill any remaining processes
    for pid in "${PIDS[@]}"; do
        if kill -0 "$pid" 2>/dev/null; then
            echo -e "${RED}Force killing process $pid${NC}"
            kill -9 "$pid" 2>/dev/null || true
        fi
    done
    
    echo -e "${GREEN}All services stopped${NC}"
    exit 0
}

# Set up trap for cleanup on script exit
trap cleanup SIGINT SIGTERM EXIT

# Check if folder path is provided
if [ -z "$1" ]; then
    echo -e "${RED}Usage: $0 <folder-path>${NC}"
    echo -e "Example: $0 ./local-chains/v30"
    echo -e "Example: $0 ./local-chains/v30/multiple-chains"
    exit 1
fi

CONFIG_DIR="$1"

# Verify the directory exists
if [ ! -d "$CONFIG_DIR" ]; then
    echo -e "${RED}Error: Directory '$CONFIG_DIR' does not exist${NC}"
    exit 1
fi

# Check for L1 state file
L1_STATE_FILE="$CONFIG_DIR/zkos-l1-state.json"
if [ ! -f "$L1_STATE_FILE" ]; then
    echo -e "${RED}Error: L1 state file '$L1_STATE_FILE' not found${NC}"
    exit 1
fi

echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}Starting Local Development Environment${NC}"
echo -e "${BLUE}Config directory: $CONFIG_DIR${NC}"
echo -e "${BLUE}========================================${NC}"

# Start Anvil
echo -e "\n${GREEN}Starting Anvil...${NC}"
anvil --load-state "$L1_STATE_FILE" --port 8545 > /dev/null 2>&1 &
ANVIL_PID=$!
PIDS+=($ANVIL_PID)
echo -e "${GREEN}Anvil started with PID $ANVIL_PID${NC}"

# Wait for Anvil to be ready
echo -e "${YELLOW}Waiting for Anvil to be ready...${NC}"
for i in {1..30}; do
    if curl -s http://localhost:8545 -X POST -H "Content-Type: application/json" \
        --data '{"jsonrpc":"2.0","method":"eth_blockNumber","params":[],"id":1}' > /dev/null 2>&1; then
        echo -e "${GREEN}Anvil is ready${NC}"
        break
    fi
    if [ $i -eq 30 ]; then
        echo -e "${RED}Anvil failed to start${NC}"
        exit 1
    fi
    sleep 1
done

# Determine which chain configs to use
SINGLE_CONFIG="$CONFIG_DIR/config.json"

if [ -f "$SINGLE_CONFIG" ]; then
    # Single chain mode
    echo -e "\n${GREEN}Starting single chain with config: $SINGLE_CONFIG${NC}"
    cargo run -- --config "$SINGLE_CONFIG" &
    CHAIN_PID=$!
    PIDS+=($CHAIN_PID)
    echo -e "${GREEN}Chain started with PID $CHAIN_PID${NC}"
else
    # Multiple chains mode - look for chain*.json files
    CHAIN_CONFIGS=($(ls "$CONFIG_DIR"/chain*.json 2>/dev/null | sort -V))
    
    if [ ${#CHAIN_CONFIGS[@]} -eq 0 ]; then
        echo -e "${RED}Error: No config.json or chain*.json files found in '$CONFIG_DIR'${NC}"
        exit 1
    fi
    
    echo -e "\n${GREEN}Starting ${#CHAIN_CONFIGS[@]} chain(s)...${NC}"
    
    for config_file in "${CHAIN_CONFIGS[@]}"; do
        echo -e "${GREEN}Starting chain with config: $config_file${NC}"
        cargo run -- --config "$config_file" &
        CHAIN_PID=$!
        PIDS+=($CHAIN_PID)
        echo -e "${GREEN}Chain started with PID $CHAIN_PID${NC}"
        
        # Small delay between starting chains to avoid port conflicts
        sleep 2
    done
fi

echo -e "\n${BLUE}========================================${NC}"
echo -e "${BLUE}All services started successfully${NC}"
echo -e "${BLUE}Press Ctrl+C to stop all services${NC}"
echo -e "${BLUE}========================================${NC}"

# Wait for all background processes
wait
