#!/bin/bash

# Configgymajiggy Deployment Script
# This script provides easy deployment and management commands

set -e

echo "🚀 Configgymajiggy Deployment Script"
echo "==========================="

case "${1:-deploy}" in
    "deploy")
        echo "📦 Building and deploying Configgymajiggy service..."
        docker compose up -d --build
        echo "✅ Service deployed successfully!"
        echo "🌍 Access your service at: http://localhost:8080"
        echo "🔍 Check health: curl http://localhost:8080/health"
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
        curl -f http://localhost:8080/health 2>/dev/null && echo " ✅ Service is healthy" || echo " ❌ Service is not responding"
        ;;
    "update")
        echo "🔄 Updating service..."
        git pull
        docker compose build
        docker compose up -d
        echo "✅ Service updated!"
        ;;
    "clean")
        echo "🧹 Cleaning up..."
        docker compose down
        docker system prune -f
        echo "✅ Cleanup complete!"
        ;;
    *)
        echo "Usage: $0 {deploy|start|stop|restart|logs|status|update|clean}"
        echo ""
        echo "Commands:"
        echo "  deploy  - Build and deploy the service (default)"
        echo "  start   - Start the service"
        echo "  stop    - Stop the service"
        echo "  restart - Restart the service"
        echo "  logs    - Show service logs"
        echo "  status  - Show service status and health"
        echo "  update  - Pull latest code and update service"
        echo "  clean   - Stop service and clean up Docker resources"
        exit 1
        ;;
esac