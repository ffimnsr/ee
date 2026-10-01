// Sample C# source file for syntax highlighting, folding, and LSP testing.

using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;

namespace EditorSamples.Workflows
{
    public enum WorkItemState
    {
        Pending,
        InProgress,
        Completed,
        Failed
    }

    public sealed record WorkItem(
        Guid Id,
        string Name,
        int Priority,
        WorkItemState State,
        DateTime CreatedAtUtc
    );

    public interface IWorkDispatcher<T> where T : class
    {
        ValueTask EnqueueAsync(T item, CancellationToken cancellationToken = default);
        Task<IReadOnlyList<T>> DrainCompletedAsync();
    }

    /// <summary>
    /// Thread-safe in-memory dispatcher processing work items asynchronously.
    /// </summary>
    public class InvertedIndexDispatcher : IWorkDispatcher<WorkItem>, IAsyncDisposable
    {
        private readonly ConcurrentQueue<WorkItem> _queue = new();
        private readonly ConcurrentBag<WorkItem> _completed = new();
        private readonly SemaphoreSlim _signal = new(0);
        private readonly CancellationTokenSource _cts = new();
        private readonly Task _workerTask;

        public InvertedIndexDispatcher(int concurrency = 2)
        {
            _workerTask = Task.Run(() => WorkerLoopAsync(_cts.Token));
        }

        public async ValueTask EnqueueAsync(WorkItem item, CancellationToken cancellationToken = default)
        {
            ArgumentNullException.ThrowIfNull(item);
            _queue.Enqueue(item);
            _signal.Release();
            await Task.Yield();
        }

        public Task<IReadOnlyList<WorkItem>> DrainCompletedAsync()
        {
            var results = new List<WorkItem>();
            while (_completed.TryTake(out var item))
            {
                results.Add(item);
            }
            return Task.FromResult<IReadOnlyList<WorkItem>>(results);
        }

        private async Task WorkerLoopAsync(CancellationToken ct)
        {
            while (!ct.IsCancellationRequested)
            {
                try
                {
                    await _signal.WaitAsync(ct);
                }
                catch (OperationCanceledException)
                {
                    break;
                }

                if (_queue.TryDequeue(out var item))
                {
                    var updated = item with { State = WorkItemState.Completed };
                    _completed.Add(updated);
                }
            }
        }

        public async ValueTask DisposeAsync()
        {
            _cts.Cancel();
            try
            {
                await _workerTask;
            }
            catch (OperationCanceledException) { }
            _signal.Dispose();
            _cts.Dispose();
        }
    }

    public static class Program
    {
        public static async Task Main(string[] args)
        {
            await using var dispatcher = new InvertedIndexDispatcher();

            var sample = new WorkItem(
                Guid.NewGuid(),
                "Analyze AST Nodes",
                10,
                WorkItemState.Pending,
                DateTime.UtcNow
            );

            await dispatcher.EnqueueAsync(sample);
            await Task.Delay(100);

            var finished = await dispatcher.DrainCompletedAsync();
            Console.WriteLine($"Processed {finished.Count} items.");
        }
    }
}
