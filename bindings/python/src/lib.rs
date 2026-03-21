use pyo3::prelude::*;
use pyo3::exceptions::PyRuntimeError;

/// Python subscriber for crossbar SHM pub/sub.
#[pyclass]
struct Subscriber {
    inner: crossbar::Subscriber,
}

#[pymethods]
impl Subscriber {
    #[new]
    fn new(name: &str) -> PyResult<Self> {
        let inner = crossbar::Subscriber::connect(name)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Self { inner })
    }

    fn subscribe(&self, uri: &str) -> PyResult<Stream> {
        let stream = self.inner.subscribe(uri)
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(Stream { inner: stream })
    }
}

#[pyclass(unsendable)]
struct Stream {
    inner: crossbar::Stream,
}

#[pymethods]
impl Stream {
    fn try_recv(&self) -> Option<Vec<u8>> {
        self.inner.try_recv().map(|s| s.to_vec())
    }

    fn recv(&self) -> PyResult<Vec<u8>> {
        let sample = self.inner.recv()
            .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;
        Ok(sample.to_vec())
    }
}

#[pymodule]
fn crossbar_python(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<Subscriber>()?;
    m.add_class::<Stream>()?;
    Ok(())
}
