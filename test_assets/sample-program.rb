# frozen_string_literal: true

# Sample Ruby program for editor syntax highlighting, code folding,
# and Ruby LSP testing.

module EditorSuite
  class MetricsCollector
    attr_reader :name, :records

    def initialize(name)
      @name = name
      @records = []
      @mutex = Thread::Mutex.new
    end

    def record(metric, value, tags = {})
      @mutex.synchronize do
        @records << {
          metric: metric.to_sym,
          value: value.to_f,
          tags: tags,
          timestamp: Time.now.utc
        }
      end
    end

    def average_for(metric)
      @mutex.synchronize do
        matching = @records.select { |r| r[:metric] == metric.to_sym }
        return 0.0 if matching.empty?

        sum = matching.sum { |r| r[:value] }
        sum / matching.size
      end
    end

    def flush!
      @mutex.synchronize do
        flushed = @records.dup
        @records.clear
        flushed
      end
    end
  end

  class PipelineWorker
    include Enumerable

    def initialize(collector, worker_count: 2)
      @collector = collector
      @worker_count = worker_count
      @queue = Thread::Queue.new
      @threads = []
    end

    def start
      @worker_count.times do |id|
        @threads << Thread.new do
          loop do
            job = @queue.pop
            break if job == :terminate

            execute_job(id, job)
          end
        end
      end
    end

    def submit(job_id, payload, &block)
      @queue.push([job_id, payload, block])
    end

    def stop
      @worker_count.times { @queue.push(:terminate) }
      @threads.each(&:join)
    end

    private

    def execute_job(worker_id, job)
      job_id, payload, block = job
      start_time = Process.clock_gettime(Process::CLOCK_MONOTONIC)

      result = block ? block.call(payload) : payload.to_s.upcase
      elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - start_time

      @collector.record(:job_duration_seconds, elapsed, worker: worker_id, job: job_id)
      result
    rescue StandardError => e
      @collector.record(:job_errors, 1.0, worker: worker_id, error: e.class.name)
    end
  end
end

if __FILE__ == $PROGRAM_NAME
  collector = EditorSuite::MetricsCollector.new("sample-app")
  worker = EditorSuite::PipelineWorker.new(collector, worker_count: 3)
  worker.start

  10.times do |n|
    worker.submit("task-#{n}", "data chunk #{n}") do |data|
      sleep(0.01 * (n % 3))
      data.reverse
    end
  end

  sleep 0.1
  worker.stop

  puts "Avg duration: #{collector.average_for(:job_duration_seconds).round(4)}s"
  puts "Collected #{collector.flush!.size} metric records."
end
