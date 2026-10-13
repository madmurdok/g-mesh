class ServiceImpl {
  start(): string {
    return "up";
  }
}
export const service = new ServiceImpl();
