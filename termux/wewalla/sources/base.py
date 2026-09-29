import threading


class Source:
    """A source pushes Sample objects into `sink(sample)` and optional IMU readings
    into `imu_sink(value)` from its own thread(s)."""
    name = "source"

    def __init__(self):
        self._stop = threading.Event()
        self._threads = []
        self.sink = lambda s: None
        self.imu_sink = lambda v: None

    def start(self, sink, imu_sink=None):
        self.sink = sink
        if imu_sink:
            self.imu_sink = imu_sink
        self._stop.clear()
        self._start()

    def _start(self):
        raise NotImplementedError

    def _spawn(self, fn, name):
        t = threading.Thread(target=fn, name=name, daemon=True)
        t.start()
        self._threads.append(t)

    def stop(self):
        self._stop.set()
        for t in self._threads:
            t.join(timeout=2)
        self._threads = []

    def info(self):
        return {"name": self.name}
