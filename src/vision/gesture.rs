use std::f64::consts::PI;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CircleGesture {
    pub center: (f64, f64),
    pub radius: f64,
}

impl CircleGesture {
    #[allow(dead_code)]
    pub fn new(center: (f64, f64), radius: f64) -> Self {
        Self { center, radius }
    }
}

#[derive(Debug, Clone, Copy)]
struct Bounds {
    min_x: f64,
    max_x: f64,
    min_y: f64,
    max_y: f64,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    point: (f64, f64),
    time: f64,
}

#[derive(Debug, Clone)]
pub struct CircleGestureDetector {
    samples: Vec<Sample>,
    cooldown_until: f64,
    waiting_for_exit: Option<CircleGesture>,
    window: f64,
    minimum_angle_degrees: f64,
    distances: Vec<f64>,
}

impl Default for CircleGestureDetector {
    fn default() -> Self {
        Self::new(315.0)
    }
}

impl CircleGestureDetector {
    pub fn new(minimum_angle_degrees: f64) -> Self {
        let minimum_angle_degrees = minimum_angle_degrees.clamp(300.0, 359.0);
        Self {
            samples: Vec::new(),
            cooldown_until: 0.0,
            waiting_for_exit: None,
            window: 6.0,
            minimum_angle_degrees,
            distances: Vec::new(),
        }
    }

    #[allow(dead_code)]
    pub fn reset(&mut self) {
        self.samples.clear();
        self.cooldown_until = 0.0;
        self.waiting_for_exit = None;
    }

    #[allow(dead_code)]
    pub fn minimum_angle_degrees(&self) -> f64 {
        self.minimum_angle_degrees
    }

    pub fn add(&mut self, point: (f64, f64), time: f64) -> Option<CircleGesture> {
        if let Some(gesture) = self.waiting_for_exit {
            let dist = (point.0 - gesture.center.0).hypot(point.1 - gesture.center.1);
            if dist <= gesture.radius * 1.5 {
                return None;
            }
            self.waiting_for_exit = None;
            self.samples.clear();
        }

        if time < self.cooldown_until {
            return None;
        }

        if let Some(previous) = self.samples.last() {
            if time - previous.time > 0.75 {
                self.samples.clear();
            }
        }

        self.samples.push(Sample { point, time });
        let cutoff = time - self.window;
        self.samples.retain(|s| s.time >= cutoff);

        if time < self.cooldown_until || self.samples.len() < 12 {
            return None;
        }

        let gesture = Self::recognized_gesture(
            &self.samples,
            self.minimum_angle_degrees,
            &mut self.distances,
        )?;

        self.samples.clear();
        self.cooldown_until = time + 0.65;
        self.waiting_for_exit = Some(gesture);
        Some(gesture)
    }

    fn recognized_gesture(
        samples: &[Sample],
        minimum_angle_degrees: f64,
        distances: &mut Vec<f64>,
    ) -> Option<CircleGesture> {
        let last = samples.last()?.point;
        let count = samples.len();
        if count < 12 {
            return None;
        }
        // Walk the start index back from the newest sample, growing the
        // bounding box of samples[start..] one point at a time.
        let (mut min_x, mut max_x) = (last.0, last.0);
        let (mut min_y, mut max_y) = (last.1, last.1);
        let mut path_length = 0.0;
        for start in (0..count).rev() {
            let first = samples[start].point;
            if let Some(next) = samples.get(start + 1) {
                path_length += (next.point.0 - first.0).hypot(next.point.1 - first.1);
            }
            min_x = min_x.min(first.0);
            max_x = max_x.max(first.0);
            min_y = min_y.min(first.1);
            max_y = max_y.max(first.1);
            if start + 12 > count {
                continue;
            }
            let closure = (first.0 - last.0).hypot(first.1 - last.1);
            if closure < 160.0 {
                let bounds = Bounds {
                    min_x,
                    max_x,
                    min_y,
                    max_y,
                };
                if let Some(gesture) = Self::evaluate_subslice(
                    &samples[start..],
                    bounds,
                    closure,
                    path_length,
                    minimum_angle_degrees,
                    distances,
                ) {
                    return Some(gesture);
                }
            }
        }
        None
    }

    fn evaluate_subslice(
        samples: &[Sample],
        bounds: Bounds,
        closure: f64,
        path_length: f64,
        minimum_angle_degrees: f64,
        distances: &mut Vec<f64>,
    ) -> Option<CircleGesture> {
        let Bounds {
            min_x,
            max_x,
            min_y,
            max_y,
        } = bounds;

        let width = max_x - min_x;
        let height = max_y - min_y;
        if width < 28.0 || height < 28.0 {
            return None;
        }

        let ratio = width / height;
        if ratio <= 0.45 || ratio >= 2.2 {
            return None;
        }

        // The mean distance to the bounding-box centre can never exceed the
        // half diagonal, so a pair of end points that far apart cannot close
        // into a loop. This skips the O(n) passes below for most starts.
        let max_radius = width.hypot(height) / 2.0 * (1.0 + 1e-9);
        if closure >= 20.0_f64.max(max_radius * 0.65) {
            return None;
        }

        // Likewise a path longer than 2.4 circumferences of that radius is
        // too long (the sums run in a different order than the exact check
        // below, hence the slack).
        if path_length * (1.0 - 1e-9) >= 2.4 * 2.0 * PI * max_radius {
            return None;
        }

        let center = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
        distances.clear();
        distances.extend(
            samples
                .iter()
                .map(|s| (s.point.0 - center.0).hypot(s.point.1 - center.1)),
        );

        let count = distances.len() as f64;
        let radius: f64 = distances.iter().sum::<f64>() / count;
        if radius < 18.0 {
            return None;
        }

        let variance: f64 = distances.iter().map(|d| (d - radius).powi(2)).sum::<f64>() / count;
        if variance.sqrt() / radius >= 0.36 {
            return None;
        }

        if closure >= 20.0_f64.max(radius * 0.65) {
            return None;
        }

        let mut angle_travel = 0.0;
        for i in 0..(samples.len() - 1) {
            let a = samples[i].point;
            let b = samples[i + 1].point;
            let current = (a.1 - center.1).atan2(a.0 - center.0);
            let next = (b.1 - center.1).atan2(b.0 - center.0);
            let mut delta = next - current;
            while delta > PI {
                delta -= 2.0 * PI;
            }
            while delta < -PI {
                delta += 2.0 * PI;
            }
            angle_travel += delta.abs();
        }

        let minimum_angle_rad = minimum_angle_degrees * PI / 180.0;
        if angle_travel <= minimum_angle_rad || angle_travel >= 8.8 {
            return None;
        }

        let mut path_length = 0.0;
        for i in 0..(samples.len() - 1) {
            let a = samples[i].point;
            let b = samples[i + 1].point;
            path_length += (b.0 - a.0).hypot(b.1 - a.1);
        }

        let circumference = 2.0 * PI * radius;
        let path_ratio = path_length / circumference;
        if path_ratio <= 0.65 || path_ratio >= 2.4 {
            return None;
        }

        Some(CircleGesture { center, radius })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_circle_threshold_clamping() {
        assert_eq!(
            CircleGestureDetector::default().minimum_angle_degrees(),
            315.0
        );
        assert_eq!(
            CircleGestureDetector::new(290.0).minimum_angle_degrees(),
            300.0
        );
        assert_eq!(
            CircleGestureDetector::new(370.0).minimum_angle_degrees(),
            359.0
        );
    }

    #[test]
    fn test_recognizes_closed_circle() {
        let mut detector = CircleGestureDetector::default();
        let mut result = None;
        let center = (300.0, 200.0);

        for index in 0..48 {
            let angle = (index as f64) / 47.0 * 2.0 * PI;
            let point = (center.0 + 52.0 * angle.cos(), center.1 + 52.0 * angle.sin());
            let time = (index as f64) / 60.0;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }

        let res = result.expect("Circle should be recognized");
        assert!((res.center.0 - center.0).abs() <= 3.0);
        assert!((res.center.1 - center.1).abs() <= 3.0);
        assert!((res.radius - 52.0).abs() <= 3.0);
    }

    #[test]
    fn test_recognizes_loose_arc_320_degrees() {
        let center = (500.0, 400.0);
        let start_angle = 0.30; // ~17.2 degrees
        let end_angle = 5.97; // ~342.0 degrees (total angular travel ~324.8 degrees / 5.67 rad)

        let generate_points = || {
            let mut points = Vec::new();
            for index in 0..40 {
                let progress = (index as f64) / 39.0;
                let angle = start_angle + progress * (end_angle - start_angle);
                let point = (center.0 + 60.0 * angle.cos(), center.1 + 60.0 * angle.sin());
                let time = (index as f64) / 40.0;
                points.push((point, time));
            }
            points
        };

        // 1. With default detector (315.0 degrees), ~325 deg loop passes
        let mut detector = CircleGestureDetector::default();
        let mut result = None;
        for (point, time) in generate_points() {
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }
        assert!(
            result.is_some(),
            "Loose ~325-degree loop should be recognized with relaxed 315.0 default"
        );

        // 2. With old threshold (340.0 degrees), ~325 deg loop fails
        let mut old_detector = CircleGestureDetector::new(340.0);
        let mut old_result = None;
        for (point, time) in generate_points() {
            if let Some(res) = old_detector.add(point, time) {
                old_result = Some(res);
            }
        }
        assert!(
            old_result.is_none(),
            "Loose ~325-degree loop should fail under strict 340.0 threshold"
        );
    }

    #[test]
    fn test_pause_tolerance_below_and_above_750ms() {
        let center = (500.0, 400.0);

        // 1. Pause of 0.60s (< 0.75s) should NOT wipe buffer and should be recognized
        let mut detector = CircleGestureDetector::default();
        let mut result = None;
        for index in 0..20 {
            let angle = (index as f64) / 39.0 * 2.0 * PI;
            let point = (center.0 + 60.0 * angle.cos(), center.1 + 60.0 * angle.sin());
            let time = (index as f64) * 0.02;
            let _ = detector.add(point, time);
        }
        let resume_time = 0.38 + 0.60;
        for index in 20..40 {
            let angle = (index as f64) / 39.0 * 2.0 * PI;
            let point = (center.0 + 60.0 * angle.cos(), center.1 + 60.0 * angle.sin());
            let time = resume_time + ((index - 20) as f64) * 0.02;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }
        assert!(
            result.is_some(),
            "Gesture with 600ms pause should be recognized"
        );

        // 2. Pause of 0.85s (> 0.75s) should wipe buffer and fail
        let mut detector2 = CircleGestureDetector::default();
        let mut result2 = None;
        for index in 0..20 {
            let angle = (index as f64) / 39.0 * 2.0 * PI;
            let point = (center.0 + 60.0 * angle.cos(), center.1 + 60.0 * angle.sin());
            let time = (index as f64) * 0.02;
            let _ = detector2.add(point, time);
        }
        let resume_time2 = 0.38 + 0.85;
        for index in 20..40 {
            let angle = (index as f64) / 39.0 * 2.0 * PI;
            let point = (center.0 + 60.0 * angle.cos(), center.1 + 60.0 * angle.sin());
            let time = resume_time2 + ((index - 20) as f64) * 0.02;
            if let Some(res) = detector2.add(point, time) {
                result2 = Some(res);
            }
        }
        assert!(
            result2.is_none(),
            "Gesture with 850ms pause should be wiped and fail"
        );
    }

    #[test]
    fn test_path_ratio_wobble_allowed_vs_excessive_rejected() {
        let center = (500.0, 400.0);

        // 1. Mild hand wobble producing path_ratio around 1.95 - 2.15 (< 2.4)
        let mut detector = CircleGestureDetector::default();
        let mut result = None;
        for index in 0..48 {
            let angle = (index as f64) / 47.0 * 2.0 * PI;
            let wobble = if index % 2 == 0 { 5.5 } else { -5.5 };
            let r = 50.0 + wobble;
            let point = (center.0 + r * angle.cos(), center.1 + r * angle.sin());
            let time = (index as f64) / 47.0;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }
        assert!(
            result.is_some(),
            "Circle with natural hand wobble (path_ratio < 2.4) should be recognized"
        );

        // 2. Severe jitter producing path_ratio >= 2.4 should be rejected
        let mut detector2 = CircleGestureDetector::default();
        let mut result2 = None;
        for index in 0..48 {
            let angle = (index as f64) / 47.0 * 2.0 * PI;
            let wobble = if index % 2 == 0 { 9.0 } else { -9.0 };
            let r = 50.0 + wobble;
            let point = (center.0 + r * angle.cos(), center.1 + r * angle.sin());
            let time = (index as f64) / 47.0;
            if let Some(res) = detector2.add(point, time) {
                result2 = Some(res);
            }
        }
        assert!(
            result2.is_none(),
            "Excessive jitter/zigzag with path_ratio >= 2.4 should be rejected"
        );
    }

    #[test]
    fn test_radius_variance_tolerance() {
        let center = (500.0, 400.0);

        // 1. Moderate non-circular distortion with variance ~0.34 (< 0.36)
        let mut detector = CircleGestureDetector::default();
        let mut result = None;
        for index in 0..64 {
            let angle = (index as f64) / 63.0 * 2.0 * PI;
            let radius = 60.0 * (1.0 + 0.48 * (angle * 2.0).sin());
            let point = (
                center.0 + radius * angle.cos(),
                center.1 + radius * angle.sin(),
            );
            let time = (index as f64) / 64.0;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }
        assert!(
            result.is_some(),
            "Loop with radius variance ~0.34 should be accepted under 0.36 tolerance"
        );

        // 2. Severe distortion with variance >= 0.36 should be rejected
        let mut detector2 = CircleGestureDetector::default();
        let mut result2 = None;
        for index in 0..64 {
            let angle = (index as f64) / 63.0 * 2.0 * PI;
            let radius = 60.0 * (1.0 + 0.58 * (angle * 2.0).sin());
            let point = (
                center.0 + radius * angle.cos(),
                center.1 + radius * angle.sin(),
            );
            let time = (index as f64) / 64.0;
            if let Some(res) = detector2.add(point, time) {
                result2 = Some(res);
            }
        }
        assert!(
            result2.is_none(),
            "Loop with radius variance >= 0.36 should be rejected"
        );
    }

    #[test]
    fn test_minimum_sample_count_12() {
        let center = (500.0, 400.0);

        // 14 samples (>= 12)
        let mut detector = CircleGestureDetector::default();
        let mut result = None;
        for index in 0..14 {
            let angle = (index as f64) / 13.0 * 2.0 * PI;
            let point = (center.0 + 50.0 * angle.cos(), center.1 + 50.0 * angle.sin());
            let time = (index as f64) * 0.015;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }
        assert!(
            result.is_some(),
            "Fast circle with 14 samples should be recognized"
        );

        // 10 samples (< 12)
        let mut detector2 = CircleGestureDetector::default();
        let mut result2 = None;
        for index in 0..10 {
            let angle = (index as f64) / 9.0 * 2.0 * PI;
            let point = (center.0 + 50.0 * angle.cos(), center.1 + 50.0 * angle.sin());
            let time = (index as f64) * 0.015;
            if let Some(res) = detector2.add(point, time) {
                result2 = Some(res);
            }
        }
        assert!(
            result2.is_none(),
            "Circle with fewer than 12 samples should not be recognized"
        );
    }

    #[test]
    fn test_recognizes_slow_loose_loop() {
        let mut detector = CircleGestureDetector::default();
        let mut result = None;
        let center = (900.0, 500.0);

        for index in 0..150 {
            let angle = (index as f64) / 149.0 * 2.0 * PI;
            let wobble = 1.0 + 0.1 * (angle * 3.0).sin();
            let point = (
                center.0 + 110.0 * wobble * angle.cos(),
                center.1 + 82.0 * wobble * angle.sin(),
            );
            let time = (index as f64) * 0.02;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }

        assert!(result.is_some(), "Slow loose loop should be recognized");
    }

    #[test]
    fn test_recognizes_slow_loose_loop_after_pointer_movement() {
        let mut detector = CircleGestureDetector::default();
        let mut result = None;

        for index in 0..60 {
            let point = (300.0 + (index as f64) * 5.0, 240.0 + ((index % 7) as f64));
            let time = (index as f64) * 0.02;
            let _ = detector.add(point, time);
        }

        let center = (900.0, 500.0);
        for index in 0..150 {
            let angle = (index as f64) / 149.0 * 2.0 * PI;
            let wobble = 1.0 + 0.1 * (angle * 3.0).sin();
            let point = (
                center.0 + 110.0 * wobble * angle.cos(),
                center.1 + 82.0 * wobble * angle.sin(),
            );
            let time = 1.2 + (index as f64) * 0.02;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }

        assert!(
            result.is_some(),
            "Circle after linear movement should be recognized"
        );
    }

    #[test]
    fn test_rejects_straight_line() {
        let mut detector = CircleGestureDetector::default();
        let mut result = None;

        for index in 0..48 {
            let point = ((index as f64) * 4.0, 200.0);
            let time = (index as f64) / 60.0;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }

        assert!(result.is_none(), "Straight line should be rejected");
    }

    #[test]
    fn test_rejects_irregular_closed_loop() {
        let mut detector = CircleGestureDetector::default();
        let mut result = None;

        for index in 0..96 {
            let angle = (index as f64) / 95.0 * 2.0 * PI;
            let radius = 70.0 * (1.0 + 0.55 * (angle * 2.0).sin());
            let point = (400.0 + radius * angle.cos(), 300.0 + radius * angle.sin());
            let time = (index as f64) / 60.0;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }

        assert!(result.is_none(), "Irregular closed loop should be rejected");
    }

    #[test]
    fn test_rejects_partial_arc() {
        let mut detector = CircleGestureDetector::default();
        let mut result = None;

        for index in 0..48 {
            let angle = (index as f64) / 47.0 * (4.0 * PI / 3.0);
            let point = (600.0 + 60.0 * angle.cos(), 400.0 + 60.0 * angle.sin());
            let time = (index as f64) / 60.0;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }

        assert!(result.is_none(), "Partial arc should be rejected");
    }

    #[test]
    fn test_rejects_hook() {
        let mut detector = CircleGestureDetector::default();
        let mut result = None;

        for index in 0..48 {
            let progress = (index as f64) / 47.0;
            let point = (
                600.0 + 100.0 * progress,
                400.0 + 60.0 * (progress * PI).sin() + 18.0 * progress,
            );
            let time = (index as f64) / 60.0;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }

        assert!(result.is_none(), "Hook gesture should be rejected");
    }

    #[test]
    fn test_rejects_zigzag() {
        let mut detector = CircleGestureDetector::default();
        let mut result = None;

        for index in 0..60 {
            let y_offset = if index % 2 == 0 { 35.0 } else { -35.0 };
            let point = (500.0 + (index as f64) * 2.0, 400.0 + y_offset);
            let time = (index as f64) / 60.0;
            if let Some(res) = detector.add(point, time) {
                result = Some(res);
            }
        }

        assert!(result.is_none(), "Zigzag should be rejected");
    }

    #[test]
    fn test_long_continuous_loop_captures_once_until_pointer_leaves() {
        let mut detector = CircleGestureDetector::default();
        let center = (400.0, 300.0);
        let mut captures = 0;

        for index in 0..240 {
            let angle = (index as f64) / 47.0 * 2.0 * PI;
            let point = (center.0 + 70.0 * angle.cos(), center.1 + 70.0 * angle.sin());
            let time = (index as f64) / 60.0;
            if detector.add(point, time).is_some() {
                captures += 1;
            }
        }

        assert_eq!(
            captures, 1,
            "Continuous loop should capture exactly once until pointer leaves center"
        );
    }

    /// The straightforward O(n^2) search: every start is evaluated from
    /// scratch. `recognized_gesture` must return exactly the same result.
    fn reference_recognized(
        samples: &[Sample],
        minimum_angle_degrees: f64,
    ) -> Option<CircleGesture> {
        let last = samples.last()?.point;
        let count = samples.len();
        if count < 12 {
            return None;
        }
        for start in (0..=(count - 12)).rev() {
            let first = samples[start].point;
            if (first.0 - last.0).hypot(first.1 - last.1) < 160.0 {
                if let Some(gesture) =
                    reference_evaluate(&samples[start..], minimum_angle_degrees)
                {
                    return Some(gesture);
                }
            }
        }
        None
    }

    fn reference_evaluate(
        samples: &[Sample],
        minimum_angle_degrees: f64,
    ) -> Option<CircleGesture> {
        let first = samples.first()?.point;
        let last = samples.last()?.point;

        let mut min_x = samples[0].point.0;
        let mut max_x = samples[0].point.0;
        let mut min_y = samples[0].point.1;
        let mut max_y = samples[0].point.1;

        for s in samples.iter().skip(1) {
            if s.point.0 < min_x {
                min_x = s.point.0;
            }
            if s.point.0 > max_x {
                max_x = s.point.0;
            }
            if s.point.1 < min_y {
                min_y = s.point.1;
            }
            if s.point.1 > max_y {
                max_y = s.point.1;
            }
        }

        let width = max_x - min_x;
        let height = max_y - min_y;
        if width < 28.0 || height < 28.0 {
            return None;
        }

        let ratio = width / height;
        if ratio <= 0.45 || ratio >= 2.2 {
            return None;
        }

        let center = ((min_x + max_x) / 2.0, (min_y + max_y) / 2.0);
        let distances: Vec<f64> = samples
            .iter()
            .map(|s| (s.point.0 - center.0).hypot(s.point.1 - center.1))
            .collect();

        let count = distances.len() as f64;
        let radius: f64 = distances.iter().sum::<f64>() / count;
        if radius < 18.0 {
            return None;
        }

        let variance: f64 = distances.iter().map(|d| (d - radius).powi(2)).sum::<f64>() / count;
        if variance.sqrt() / radius >= 0.36 {
            return None;
        }

        let closure = (first.0 - last.0).hypot(first.1 - last.1);
        if closure >= 20.0_f64.max(radius * 0.65) {
            return None;
        }

        let mut angle_travel = 0.0;
        for i in 0..(samples.len() - 1) {
            let a = samples[i].point;
            let b = samples[i + 1].point;
            let current = (a.1 - center.1).atan2(a.0 - center.0);
            let next = (b.1 - center.1).atan2(b.0 - center.0);
            let mut delta = next - current;
            while delta > PI {
                delta -= 2.0 * PI;
            }
            while delta < -PI {
                delta += 2.0 * PI;
            }
            angle_travel += delta.abs();
        }

        let minimum_angle_rad = minimum_angle_degrees * PI / 180.0;
        if angle_travel <= minimum_angle_rad || angle_travel >= 8.8 {
            return None;
        }

        let mut path_length = 0.0;
        for i in 0..(samples.len() - 1) {
            let a = samples[i].point;
            let b = samples[i + 1].point;
            path_length += (b.0 - a.0).hypot(b.1 - a.1);
        }

        let circumference = 2.0 * PI * radius;
        let path_ratio = path_length / circumference;
        if path_ratio <= 0.65 || path_ratio >= 2.4 {
            return None;
        }

        Some(CircleGesture { center, radius })
    }

    #[test]
    fn test_recognized_gesture_matches_reference_search() {
        let mut state = 0x9e37_79b9_7f4a_7c15_u64;
        let mut rand = move |lo: f64, hi: f64| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            lo + (hi - lo) * ((state >> 33) as f64 / (1u64 << 31) as f64)
        };
        let mut distances = Vec::new();
        let mut recognized = 0;
        for case in 0..120 {
            let minimum_angle = [315.0, 300.0, 340.0, 359.0][case % 4];
            let center = (rand(100.0, 900.0), rand(100.0, 700.0));
            let radius = rand(10.0, 200.0);
            let noise = rand(0.0, 0.6) * radius;
            let step = rand(0.05, 0.5);
            let mut angle = rand(0.0, 2.0 * PI);
            let mut walker = center;
            let mut trace = Vec::new();
            for index in 0..160 {
                let point = match case % 5 {
                    0 => {
                        angle += step;
                        let r = radius + rand(-noise, noise);
                        (center.0 + r * angle.cos(), center.1 + r * angle.sin())
                    }
                    1 => {
                        walker = (walker.0 + rand(-30.0, 30.0), walker.1 + rand(-30.0, 30.0));
                        walker
                    }
                    2 => (center.0 + rand(0.0, 160.0), center.1 + rand(0.0, 160.0)),
                    3 => {
                        angle += step;
                        let squash = rand(0.3, 2.0);
                        (
                            center.0 + radius * squash * angle.cos(),
                            center.1 + radius * angle.sin(),
                        )
                    }
                    _ => {
                        walker = (walker.0 + rand(-3.0, 3.0), walker.1 + rand(-3.0, 3.0));
                        walker
                    }
                };
                trace.push(Sample {
                    point,
                    time: index as f64 / 60.0,
                });
            }
            for end in 12..=trace.len() {
                let window = &trace[end.saturating_sub(100)..end];
                let expected = reference_recognized(window, minimum_angle);
                let actual = CircleGestureDetector::recognized_gesture(
                    window,
                    minimum_angle,
                    &mut distances,
                );
                assert_eq!(actual, expected, "case {case} window ending at {end}");
                recognized += usize::from(expected.is_some());
            }
        }
        assert!(
            recognized > 50,
            "fixture should include recognised gestures, got {recognized}"
        );
    }
}
