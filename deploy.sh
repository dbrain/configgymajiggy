#!/bin/bash

# Configgymajiggy Deployment Script
# This script provides easy deployment and management commands

set -Eeuo pipefail

# shellcheck disable=SC1091
[ -f .env ] && . ./.env
PORT="${EXTERNAL_PORT:-8080}"

echo "🚀 Configgymajiggy Deployment Script"
echo "==========================="

case "${1:-deploy}" in
    "deploy")
        echo "📦 Building and deploying Configgymajiggy service..."
        docker compose up -d --build
        echo "✅ Service deployed successfully!"
        echo "🌍 Access your service at: http://localhost:${PORT}"
        echo "🔍 Check health: curl http://localhost:${PORT}/health"
        ;;
    "start")
        echo "▶️  Starting Configgymajiggy service..."
        docker compose up -d
        echo "✅ Service started!"
        ;;
    "stop")
        echo "⏹️  Stopping Configgymajiggy service..."
        docker compose down
        echo "✅ Service stopped!"
        ;;
    "restart")
        echo "🔄 Restarting Configgymajiggy service..."
        docker compose restart
        echo "✅ Service restarted!"
        ;;
    "logs")
        echo "📋 Showing service logs..."
        docker compose logs -f configgymajiggy
        ;;
    "status")
        echo "📊 Service status:"
        docker compose ps
        echo ""
        echo "🔍 Health check:"
        if curl -fsS "http://localhost:${PORT}/health" >/dev/null 2>&1; then
            echo " ✅ Service is healthy"
        else
            echo " ❌ Service is not responding on port ${PORT}"
        fi
        ;;
    "update")
        echo "🔄 Updating service..."
        if [ -n "$(git status --porcelain)" ]; then
            echo "❌ Working tree is dirty. Commit or stash before updating." >&2
            exit 1
        fi
        git pull --ff-only
        docker compose up -d --build
        echo "✅ Updated to $(git rev-parse --short HEAD): $(git log -1 --format=%s)"
        ;;
    "clean")
        # Scoped to this project only - never `docker system prune`, which would
        # also delete other projects' containers, networks and build cache.
        echo "🧹 Cleaning up..."
        docker compose down --rmi local --volumes --remove-orphans
        echo "✅ Cleanup complete!"
        ;;
    "help"|"-h"|"--help")
        echo "Usage: $0 {deploy|start|stop|restart|logs|status|update|clean}"
        ;;
    *)
        echo "Usage: $0 {deploy|start|stop|restart|logs|status|update|clean}" >&2
        exit 1
        ;;
esac
