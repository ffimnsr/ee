"""Sample Python program for editor syntax highlighting, code folding, and LSP testing."""

from __future__ import annotations

import asyncio
import functools
import logging
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from typing import Any, Callable, Dict, Generic, List, Optional, TypeVar

logging.basicConfig(level=logging.INFO, format="%(asctime)s [%(levelname)s] %(message)s")
logger = logging.getLogger(__name__)

T = TypeVar("T")


def timed_execution(label: str) -> Callable:
    """Decorator to measure and log function execution time."""
    def decorator(func: Callable) -> Callable:
        @functools.wraps(func)
        async def async_wrapper(*args: Any, **kwargs: Any) -> Any:
            start = datetime.now(timezone.utc)
            logger.info("Starting %s (%s)...", func.__name__, label)
            try:
                return await func(*args, **kwargs)
            finally:
                elapsed = (datetime.now(timezone.utc) - start).total_seconds()
                logger.info("Finished %s in %.4f seconds", func.__name__, elapsed)

        @functools.wraps(func)
        def sync_wrapper(*args: Any, **kwargs: Any) -> Any:
            start = datetime.now(timezone.utc)
            try:
                return func(*args, **kwargs)
            finally:
                elapsed = (datetime.now(timezone.utc) - start).total_seconds()
                logger.info("Finished %s in %.4f seconds", func.__name__, elapsed)

        return async_wrapper if asyncio.iscoroutinefunction(func) else sync_wrapper

    return decorator


@dataclass
class TaskItem(Generic[T]):
    """Represents a discrete unit of work within the scheduler."""

    task_id: str
    payload: T
    priority: int = 0
    retries: int = 3
    tags: List[str] = field(default_factory=list)
    metadata: Dict[str, Any] = field(default_factory=dict)
    created_at: datetime = field(default_factory=lambda: datetime.now(timezone.utc))

    def is_urgent(self) -> bool:
        """Return True if the task has a priority strictly greater than 10."""
        return self.priority > 10

    def add_tag(self, tag: str) -> None:
        """Add a unique tag to the task item."""
        if tag not in self.tags:
            self.tags.append(tag)


class TaskQueue(Generic[T]):
    """An asynchronous task queue with retry logic and lifecycle management."""

    def __init__(self, name: str, max_concurrency: int = 4) -> None:
        self.name = name
        self.max_concurrency = max_concurrency
        self._queue: asyncio.Queue[TaskItem[T]] = asyncio.Queue()
        self._active_workers: List[asyncio.Task[None]] = []
        self._running = False
        self._completed_count = 0
        self._failed_count = 0

    @property
    def stats(self) -> Dict[str, int]:
        """Summary metrics of processed items."""
        return {
            "queued": self._queue.qsize(),
            "completed": self._completed_count,
            "failed": self._failed_count,
        }

    async def enqueue(self, item: TaskItem[T]) -> None:
        """Add a task item to the processing queue."""
        logger.debug("Enqueuing task %s into queue '%s'", item.task_id, self.name)
        await self._queue.put(item)

    async def _worker_loop(self, worker_id: int) -> None:
        """Internal worker consumer loop."""
        while self._running:
            try:
                task = await asyncio.wait_for(self._queue.get(), timeout=1.0)
            except asyncio.TimeoutError:
                continue

            try:
                await self._process_task(worker_id, task)
                self._completed_count += 1
            except Exception as exc:
                logger.error("Worker %d failed processing %s: %s", worker_id, task.task_id, exc)
                if task.retries > 0:
                    task.retries -= 1
                    logger.warning("Re-enqueuing task %s (retries left: %d)", task.task_id, task.retries)
                    await self._queue.put(task)
                else:
                    self._failed_count += 1
            finally:
                self._queue.task_done()

    async def _process_task(self, worker_id: int, task: TaskItem[T]) -> None:
        """Simulate processing of a task item."""
        logger.info("Worker %d processing task '%s' (prio=%d)", worker_id, task.task_id, task.priority)
        # Simulate processing delay
        delay = 0.05 if task.is_urgent() else 0.1
        await asyncio.sleep(delay)

    @timed_execution("Queue Lifecycle")
    async def start(self) -> None:
        """Start worker tasks and run until cancellation."""
        self._running = True
        logger.info("Starting %d workers for queue '%s'", self.max_concurrency, self.name)
        for i in range(self.max_concurrency):
            worker = asyncio.create_task(self._worker_loop(i + 1))
            self._active_workers.append(worker)

    async def stop(self) -> None:
        """Gracefully stop worker tasks after emptying the queue."""
        logger.info("Draining queue '%s'...", self.name)
        await self._queue.join()
        self._running = False
        for worker in self._active_workers:
            worker.cancel()
        await asyncio.gather(*self._active_workers, return_exceptions=True)
        self._active_workers.clear()
        logger.info("Queue '%s' stopped cleanly: %s", self.name, self.stats)


async def main() -> int:
    """Entrypoint demonstration."""
    queue: TaskQueue[str] = TaskQueue("sample-queue", max_concurrency=3)
    await queue.start()

    sample_tasks = [
        TaskItem(task_id=f"task-{i:03d}", payload=f"payload-data-{i}", priority=i % 12)
        for i in range(15)
    ]

    for task in sample_tasks:
        if task.priority > 8:
            task.add_tag("high-priority")
        await queue.enqueue(task)

    await queue.stop()
    return 0


if __name__ == "__main__":
    sys.exit(asyncio.run(main()))
