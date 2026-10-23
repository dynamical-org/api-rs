use crate::Error;

#[derive(Debug)]
pub(crate) struct Axis<'a> {
    pub(crate) values: &'a [f64],
    lower: f64,
    upper: f64,
}

impl<'a> Axis<'a> {
    pub(crate) fn new(values: &'a [f64]) -> Result<Self, Error> {
        let first = *values.first().ok_or(Error::EmptyAxis)?;
        if values.iter().any(|value| !value.is_finite()) {
            return Err(Error::NonFiniteAxis);
        }
        let ascending = values
            .windows(2)
            .find(|w| w[0] != w[1])
            .is_none_or(|w| w[0] < w[1]);
        if values
            .windows(2)
            .any(|w| if ascending { w[0] > w[1] } else { w[0] < w[1] })
        {
            return Err(Error::NonMonotonicAxis);
        }
        let last = values[values.len() - 1];
        let (minimum, maximum) = if ascending {
            (first, last)
        } else {
            (last, first)
        };
        if !(maximum - minimum).is_finite() {
            return Err(Error::InvalidAxisExtent);
        }
        let mut steps: Vec<_> = values
            .windows(2)
            .filter(|w| w[0] != w[1])
            .map(|w| (w[1] - w[0]).abs())
            .collect();
        steps.sort_by(f64::total_cmp);
        let spacing = match steps.len() {
            0 => 0.0,
            n if n % 2 == 1 => steps[n / 2],
            n => (steps[n / 2 - 1] + steps[n / 2]) / 2.0,
        };
        let lower = minimum - spacing / 2.0;
        let upper = maximum + spacing / 2.0;
        if !lower.is_finite() || !upper.is_finite() {
            return Err(Error::InvalidAxisExtent);
        }
        Ok(Self {
            values,
            lower,
            upper,
        })
    }

    pub(crate) fn nearest(&self, value: f64) -> Result<usize, Error> {
        if !value.is_finite() {
            return Err(Error::InvalidCoordinate);
        }
        if value < self.lower || value > self.upper {
            return Err(Error::OutsideGrid);
        }
        let mut best = (0, f64::INFINITY);
        for (index, coordinate) in self.values.iter().enumerate() {
            let distance = (coordinate - value).abs();
            if distance < best.1 {
                best = (index, distance);
            }
        }
        Ok(best.0)
    }
}
