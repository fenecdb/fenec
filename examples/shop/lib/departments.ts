export const DEPARTMENTS = ['Shelter and sleep', 'Carry', 'Water and kitchen', 'Light and power', 'Wear', 'Navigation and safety', 'Camp furniture'];
export const departmentId = (d: string) => d.toLowerCase().replace(/[^a-z]+/g, '-');
