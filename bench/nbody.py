import math
class Body:
    def __init__(s,x,y,z,vx,vy,vz,m): s.x,s.y,s.z,s.vx,s.vy,s.vz,s.m=x,y,z,vx,vy,vz,m
def energy(bs):
    e=0.0;n=len(bs)
    for i in range(n):
        b=bs[i]; e+=0.5*b.m*(b.vx*b.vx+b.vy*b.vy+b.vz*b.vz)
        for j in range(i+1,n):
            c=bs[j];dx=b.x-c.x;dy=b.y-c.y;dz=b.z-c.z
            e-=b.m*c.m/math.sqrt(dx*dx+dy*dy+dz*dz)
    return e
def advance(bs,dt):
    n=len(bs)
    for i in range(n):
        b=bs[i]
        for j in range(i+1,n):
            c=bs[j];dx=b.x-c.x;dy=b.y-c.y;dz=b.z-c.z
            d2=dx*dx+dy*dy+dz*dz;mag=dt/(d2*math.sqrt(d2))
            b.vx-=dx*c.m*mag;b.vy-=dy*c.m*mag;b.vz-=dz*c.m*mag
            c.vx+=dx*b.m*mag;c.vy+=dy*b.m*mag;c.vz+=dz*b.m*mag
    for b in bs: b.x+=dt*b.vx;b.y+=dt*b.vy;b.z+=dt*b.vz
bs=[Body(0.0,0.0,0.0,0.0,0.0,0.0,39.47),Body(4.84,-1.16,-0.10,0.60,2.81,-0.02,0.037),Body(8.34,4.12,-0.40,-1.01,1.82,0.008,0.011),Body(12.89,-15.11,-0.22,1.08,0.86,-0.01,0.0017),Body(15.37,-25.91,0.17,0.97,0.59,-0.03,0.002)]
print(round(energy(bs),6))
for _ in range(200000): advance(bs,0.01)
print(round(energy(bs),6))
