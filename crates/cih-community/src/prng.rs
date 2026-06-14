pub struct Mulberry32 {
    state: u32,
}

impl Mulberry32 {
    pub fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    pub fn next_f64(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6d2b79f5);
        let mut t = self.state ^ (self.state >> 15);
        t = t.wrapping_mul(1 | self.state);
        t ^= t.wrapping_add(t.wrapping_mul(61 | t));
        t ^= t >> 14;
        (t as f64) / 4_294_967_296.0
    }

    pub fn shuffle<T>(&mut self, v: &mut [T]) {
        if v.len() <= 1 {
            return;
        }
        for i in (1..v.len()).rev() {
            let j = (self.next_f64() * ((i + 1) as f64)).floor() as usize;
            v.swap(i, j.min(i));
        }
    }
}
