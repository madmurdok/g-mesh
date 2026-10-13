package meter

type Meter struct{ v int }

func (m *Meter) read() int { return m.v }
